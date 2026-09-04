#!/usr/bin/env bash
#
# The linker seam of the build gate (issue #863).
#
# `.cargo/config.toml` points `target.x86_64-unknown-linux-gnu.linker` here, so
# rustc runs this instead of `cc` for every link on Linux. It takes one slot
# from the machine-wide `link` pool (see `scripts/build-gate.sh`) and then execs
# the real linker driver with rustc's argv untouched.
#
# WHY THE LINKER, AND NOT THE WHOLE BUILD. Issue #863's storm was measured as
# 22 concurrent linkers, `dm-0` at 100%, and `ld invoked oom-killer`; the CPU
# was idle. Linking is where a Rust workspace's I/O and memory actually go —
# this workspace links ~100 test binaries, each of which statically pulls the
# whole dependency graph's debug info — while compilation is CPU-bound and
# cheap to run wide. Gating whole `cargo` invocations would have bounded the
# storm too, but by serialising builds that mostly are not linking: a second
# agent would wait out a ten-minute compile to reach a link it could have run
# immediately. Gating links bounds the resource that ran out and leaves
# compilation unbounded.
#
# What that costs a SINGLE build is not established. The measurements behind
# this were taken on a box with two other agents building unbounded throughout,
# and their load varied about fivefold between runs, so wall-clock time never
# separated from them; a rebuild that is nothing but links is in any case the
# worst case for a bound on links. What did reproduce is the footprint: 60
# concurrent linker processes holding 8.3-8.6 GB ungated, against 24 holding
# 3.8-4.1 GB at six slots, twice each. See docs/develop/build-gate.md.
#
# WHY NOT CARGO'S OWN KNOBS. `-j` / `build.jobs` bound one invocation; the
# problem is the total across invocations that do not know about each other.
# rustc has no link-job limit of its own, and there is no jobserver shared
# between unrelated cargo processes, so the limit has to be built.
#
# DEGRADATION. If the gate script is missing or not executable — a partial
# checkout, a `noexec` mount — this execs the real linker directly rather than
# failing the link. Everything past that point degrades inside
# `build-gate.sh`, which never fails a command it was asked to run.
#
# ESCAPE HATCHES.
#   DAD_LINK_JOBS=0   run every link ungated (the kill switch)
#   DAD_LINK_JOBS=N   use N link slots instead of the computed default
#   DAD_LINKER=clang  use a different linker driver (this config entry
#                     overrides whatever `linker` a personal
#                     `~/.cargo/config.toml` sets, so this puts it back)
set -u

linker="${DAD_LINKER:-cc}"

here="$(dirname -- "$0")"
gate="$here/build-gate.sh"
[ -x "$gate" ] || exec "$linker" "$@"

# The default is derived from the machine rather than pinned to the 16-core box
# this was measured on. Memory is the binding constraint — the failure was an
# OOM, and each linker on this workspace holds hundreds of MB — so the budget
# is one slot per 4 GiB of RAM, never more slots than there are cores (past
# that the box cannot run them anyway) and never fewer than one (a zero would
# read as "disabled"). On the 27 GiB / 16-core box in the report that is 6,
# against the 22 concurrent linkers measured during the incident. On a 4-core /
# 16 GiB CI runner it resolves to 4, against cargo's own default `-j 4`, so the
# gate should be inert there — arithmetic on a runner spec, not a measurement.
jobs="${DAD_LINK_JOBS:-}"
if [ -z "$jobs" ]; then
    ncpu="$(nproc 2>/dev/null || echo 1)"
    memkb="$(awk '/^MemTotal:/{print $2; exit}' /proc/meminfo 2>/dev/null || echo 0)"
    jobs=$(( memkb / 4194304 ))
    [ "$jobs" -lt 1 ] && jobs=1
    [ "$jobs" -gt "$ncpu" ] && jobs="$ncpu"
fi

exec "$gate" --pool link --jobs "$jobs" -- "$linker" "$@"
