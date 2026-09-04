#!/usr/bin/env bash
#
# A cross-process, cross-worktree slot semaphore for build work (issue #863).
#
# Usage: build-gate.sh --pool NAME [--jobs N] [--wait SECONDS] -- CMD [ARG...]
#
# Acquires one of N slots in a named pool, then runs CMD with the slot held,
# and releases it when CMD exits. The slots live in a fixed directory outside
# any worktree, so every checkout, worktree and clone on the machine draws
# from the SAME pool — which is the point: the thing that has to be bounded is
# concurrency across worktrees, not within one build.
#
# WHY THIS EXISTS. Measured on 2026-09-03 (issue #863): two concurrent
# workspace builds from separate dispatch worktrees, plus a third starting
# later, drove PSI `io full avg300=65.95` — for two-thirds of a five-minute
# window EVERY runnable task on the box was blocked on disk — with `dm-0` at
# 100% utilisation and 22 concurrent linkers. `ld` then invoked the OOM killer
# and took out `systemd-resolve`, `apparmor_parser`, `upowerd` and
# `wpa_supplicant`. The CPU was idle throughout (`cpu some avg300=0.08`), so
# this is an I/O and memory bound, not a CPU one, and cargo's own `-j` cannot
# express it: `-j` is per-invocation, and nothing coordinates invocations.
#
# WHY `flock`, AND WHY THERE IS NO STALE-LOCK PATH TO HANDLE. `flock(2)` locks
# are held by an open file description and released by the kernel when the last
# descriptor closes — including on SIGKILL, on OOM-kill, and on a power-cut
# reboot. A slot can therefore never be left held by a process that is gone,
# which is the failure mode a lock FILE (a pid written into a file, removed on
# exit) has and this does not. Nothing here reads, writes, validates or expires
# a lock's contents; the slot files stay empty forever and are never unlinked.
#
# HOW IT DEGRADES. Every rung below runs CMD **ungated** rather than failing:
# no `flock` on PATH, a lock directory that cannot be created or written, a
# `--jobs` value of 0 or `off`, an unparseable `--jobs`, or the whole-run wait
# budget expiring. The gate is an optimisation on a shared box, so the only
# outcome it must never produce is a build that fails, or one that hangs, for
# a reason the gate invented. The wait budget is what bounds the hang: after
# `--wait` seconds without a slot it warns on stderr and proceeds anyway, so
# even a pool wedged by some future bug costs a delay, never a red build.
#
# STARVATION. Waiters scan the slots from a random offset and, when all are
# busy, block on a randomly chosen one with a short timeout before rescanning.
# So no waiter queues permanently behind one long-held slot while another frees,
# and no waiter can be pinned to a slot whose holder outlives every other.
set -u

pool=""
jobs=""
wait_budget="${DAD_BUILD_GATE_WAIT:-900}"

while [ $# -gt 0 ]; do
    case "$1" in
        --pool) pool="${2:-}"; shift 2 || exit 2 ;;
        --jobs) jobs="${2:-}"; shift 2 || exit 2 ;;
        --wait) wait_budget="${2:-}"; shift 2 || exit 2 ;;
        --) shift; break ;;
        *)
            echo "build-gate.sh: unknown option '$1'" >&2
            echo "usage: build-gate.sh --pool NAME [--jobs N] [--wait S] -- CMD [ARG...]" >&2
            exit 2
            ;;
    esac
done

if [ $# -eq 0 ]; then
    echo "build-gate.sh: no command given (did you forget '--'?)" >&2
    echo "usage: build-gate.sh --pool NAME [--jobs N] [--wait S] -- CMD [ARG...]" >&2
    exit 2
fi

if [ -z "$pool" ]; then
    echo "build-gate.sh: --pool is required" >&2
    exit 2
fi

# The pool name becomes one path component under the gate directory, so keep it
# to characters that cannot climb out of it. Callers here pass a literal, but a
# `--pool ../..` would otherwise turn a lock into a write somewhere else.
case "$pool" in
    *[!A-Za-z0-9_-]*)
        echo "build-gate.sh: --pool '$pool' must be [A-Za-z0-9_-]" >&2
        exit 2
        ;;
esac

# ---------------------------------------------------------------------------
# From here on, EVERY failure path runs the command instead of reporting an
# error. Argument errors above are the only exceptions, because they are the
# caller's bug and are caught the first time the wrapper is exercised.
# ---------------------------------------------------------------------------

# `off`, `0` and a non-numeric value all mean "do not gate". A non-numeric one
# is a misconfiguration, so it says so — but it still runs the build.
case "$jobs" in
    off | 0 | "") exec "$@" ;;
    *[!0-9]*)
        echo "build-gate.sh: --jobs '$jobs' is not a number; running ungated" >&2
        exec "$@"
        ;;
esac

# A cap far above any plausible core count is not a cap, and honouring it
# literally would create that many files before discovering as much. Treat it
# as the "no bound" it effectively is.
if [ "$jobs" -gt 1024 ]; then
    exec "$@"
fi
case "$wait_budget" in
    "" | *[!0-9]*) wait_budget=900 ;;
esac

command -v flock >/dev/null 2>&1 || exec "$@"

# Default location: a FIXED absolute path, deliberately not `$TMPDIR` and not
# `$XDG_RUNTIME_DIR`. The pool's entire value is that every build on the box
# finds the SAME one, and both of those vary per process — this repository's own
# e2e harness relocates `TMPDIR`, and `XDG_RUNTIME_DIR` is per login session —
# so deriving the path from either would silently hand two agents two private
# pools that each look like they are working. Not a worktree either, for the
# same reason. The `$(id -u)` suffix keeps two users on one machine from sharing
# a pool, or fighting over its permissions.
#
# If that directory cannot be made usable the gate does NOT quietly fall back to
# somewhere else: a pool at a path other builds will not look in is worse than
# no pool, because it bounds nothing while reporting success. It degrades to an
# ungated run instead, like every other rung.
gate_base="${DAD_BUILD_GATE_DIR:-/tmp/dad-build-gate-$(id -u 2>/dev/null || echo 0)}"
gate_dir="$gate_base/$pool"
# `umask 077` so both levels are created private. On a world-writable `/tmp`
# the path below is guessable, and anything another user can put there we would
# otherwise open.
(umask 077 && mkdir -p "$gate_dir") 2>/dev/null || exec "$@"

# REFUSE A POOL WE DO NOT OWN. `/tmp` is world-writable and the default path
# embeds only our uid, so another local user can create it first. `-O` is the
# check that matters: a squatted directory is owned by whoever made it, so this
# degrades to an ungated run instead of trusting its contents. Combined with the
# private umask above, a base we do own cannot then be written by anyone else.
for d in "$gate_base" "$gate_dir"; do
    [ -d "$d" ] || exec "$@"
    [ -O "$d" ] || exec "$@"
done
[ -w "$gate_dir" ] || exec "$@"

# The slot files are created once and then only ever locked. `flock` on a
# non-existent path would create it itself, but pre-creating keeps a failed
# creation (a full or read-only filesystem) on the degrade path rather than
# turning into a per-attempt error.
#
# EVERY SLOT MUST BE A PLAIN FILE, and this is checked rather than assumed
# because the failure it prevents is a HANG rather than an error. Opening a
# FIFO for write blocks in `open(2)` until a reader arrives — before the wait
# budget below exists, and outside anything `flock -w` can time out, since that
# timeout covers the lock and not the open. So a single FIFO at one of these
# paths would wedge every link on the machine forever, which is the one outcome
# the whole degradation ladder exists to rule out. A symlink is refused for the
# same reason: `-f` resolves it, so it could name a FIFO elsewhere.
slot=0
while [ "$slot" -lt "$jobs" ]; do
    path="$gate_dir/slot.$slot"
    if [ -e "$path" ] || [ -L "$path" ]; then
        { [ -f "$path" ] && [ ! -L "$path" ]; } || exec "$@"
    fi
    : >>"$path" 2>/dev/null || exec "$@"
    slot=$((slot + 1))
done

# `flock -n` exits with this when the lock is held by someone else. Chosen to
# be outside the range a linker or a cargo invocation realistically returns; a
# command that genuinely exits 213 is re-run on another slot, which costs time
# and never changes the result.
busy=213

deadline=$(( $(date +%s) + wait_budget ))
offset=$(( RANDOM % jobs ))

while :; do
    k=0
    while [ "$k" -lt "$jobs" ]; do
        i=$(( (offset + k) % jobs ))
        flock -n -E "$busy" "$gate_dir/slot.$i" "$@"
        rc=$?
        [ "$rc" -ne "$busy" ] && exit "$rc"
        k=$((k + 1))
    done

    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "build-gate.sh: no '$pool' slot after ${wait_budget}s; running ungated" >&2
        exec "$@"
    fi

    # Every slot was busy on that pass. Block on one of them so this waiter is
    # woken by a release rather than spinning, but with a timeout so it goes
    # back to scanning if a DIFFERENT slot frees first.
    offset=$(( RANDOM % jobs ))
    flock -w 2 -E "$busy" "$gate_dir/slot.$offset" "$@"
    rc=$?
    [ "$rc" -ne "$busy" ] && exit "$rc"
done
