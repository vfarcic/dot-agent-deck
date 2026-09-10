#!/usr/bin/env bash
#
# Whole-box sampler for a task-cost measurement run (issue #906).
#
# Protocol: docs/develop/measuring-task-cost.md
#
# Run by the COORDINATOR, not by a measuring agent. An agent can see only its
# own processes; at N>1 the numbers that matter -- total memory across every
# agent, machine-wide I/O pressure, link-pool queue depth, disk growth -- have
# no per-agent view at all. `/usr/bin/time -v`'s max RSS is the largest single
# process, never a sum, which is exactly the gap this fills.
#
# Emits one epoch-stamped line per interval so samples correlate with the UTC
# timestamps agents record per gate.
#
# Usage: sample-box.sh [--interval SECONDS] [--out FILE] [--label TEXT]
#
# Reads /proc and shells out to ps, pgrep, df, fuser and awk -- all read-only
# and all cheap. (`pgrep` counts the pools, `fuser` reads the link slots and is
# optional; see the WARNING it prints when absent.) What matters for a sampler
# is the next sentence, not the exact list: it starts no build, holds no link
# slot and writes nothing outside --out, so it does not perturb what it
# measures.

set -u

interval=5
out="box-samples.tsv"
label=""

while [ $# -gt 0 ]; do
    case "$1" in
        --interval) interval="${2:?--interval needs a value}"; shift 2 ;;
        --out)      out="${2:?--out needs a value}"; shift 2 ;;
        --label)    label="${2:?--label needs a value}"; shift 2 ;;
        -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "sample-box.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$interval" in
    ''|*[!0-9]*) echo "sample-box.sh: --interval must be a positive integer" >&2; exit 2 ;;
    0) echo "sample-box.sh: --interval must be greater than 0" >&2; exit 2 ;;
esac

# The link pool build-gate.sh uses. Occupancy is counted as slot files held;
# queue depth is gated processes minus that, NOT linkers minus that -- see
# gate_procs below for why `ld` cannot see a waiter.
pool="${DAD_BUILD_GATE_DIR:-/tmp/dad-build-gate-$(id -u)}/link"

# $1 = cpu|io|memory, $2 = some|full -> avg10 as a bare number.
# The lines read `some avg10=... avg60=...` with NO trailing colon on the
# first field; matching "some:" silently yields an empty column, which is how
# the first version of this script produced blank PSI for every sample.
psi() {
    local v
    v=$(awk -v want="$2" '$1 == want { for (i = 2; i <= NF; i++)
        if ($i ~ /^avg10=/) { sub(/^avg10=/, "", $i); print $i; exit } }' \
        "/proc/pressure/$1" 2>/dev/null)
    # `cpu full` is always 0 outside a cgroup, and older kernels omit the line
    # entirely; NA distinguishes "not reported" from a real zero.
    printf '%s' "${v:-NA}"
}

# Sum RSS in kB over every process whose comm matches the toolchain. This is
# the number no agent can report.
#
# A LOWER BOUND, not a total, and the column is named that way. comm matching
# cannot be exhaustive: a name this list does not carry contributes nothing and
# looks identical to an idle box. Known gaps left uncounted on purpose -- a
# versioned driver (`gcc-15`), `rust-lld`, `ld.gold`, a distro wrapper script,
# and the gate's own `bash` processes.
#
# THE DRIVER IS `gcc`, NOT `cc`, and that is measured rather than assumed.
# `link-gate.sh` execs `$DAD_LINKER` (default `cc`), but `cc` is a symlink to
# `gcc` and the wrapper re-execs the real binary, so the live process reports
# `comm=gcc` while its argv still says `cc`. Matching only `cc` -- the obvious
# reading -- therefore matches NOTHING on this box. Both names are listed
# because `DAD_LINKER=clang` is a supported override, and the driver holds real
# memory at the head of every gated link pipeline.
toolchain_rss_kb() {
    ps -eo comm=,rss= 2>/dev/null | awk '
        $1 == "rustc" || $1 == "cargo" || $1 == "ld" || $1 == "ld.lld" ||
        $1 == "collect2" || $1 == "cc1" || $1 == "cc1plus" || $1 == "mold" ||
        $1 == "cc" || $1 == "gcc" || $1 == "clang" ||
        $1 == "c++" || $1 == "g++" || $1 == "clang++" { s += $2 }
        END { print s + 0 }'
}

count() { pgrep -x "$1" 2>/dev/null | wc -l | tr -d ' '; }

# Count the gate's own processes for a pool: every build-gate.sh that is either
# holding a slot or waiting for one.
#
# WHY THIS EXISTS, AND WHY `ld` CANNOT ANSWER IT. build-gate.sh acquires a slot
# BEFORE it runs the command, so a link waiting for a slot has spawned no
# driver, no `collect2` and no `ld` -- measured: a waiting gate process has no
# children at all between its 2s `flock` retries. So `ld` minus slots held is
# structurally incapable of counting a waiter, and in the contended case the
# metric exists to catch it reads 0. Counting the gate processes themselves is
# the view that does see them, because the bash process persists for the whole
# wait and for the whole run.
#
# MATCHED ON argv, NOT comm, and anchored. A `#!/usr/bin/env bash` script
# reports `comm=bash`, so `pgrep -x build-gate.sh` returns 0 -- measured. A
# bare `pgrep -f build-gate.sh` over-counts instead: it matches any shell whose
# own command line merely mentions the script, including the coordinator's.
# Anchoring to `<interpreter> <path>/build-gate.sh ... --pool <pool>` and
# requiring the pool to match excludes both. Verified against a live pool: 0
# with nothing gated, 1 for a lone holder, 3 for one holder plus two waiters,
# and unchanged by a concurrent `--pool other`.
gate_procs() {
    ps -eo args= 2>/dev/null | awk -v pool="$1" '
        $0 ~ ("^[^ ]*(ba)?sh[ \t]+[^ ]*build-gate\\.sh[ \t]+.*--pool[ \t]+" pool "([ \t]|$)") { n++ }
        END { print n + 0 }'
}

header='epoch	iso	label	load1	memavail_kB	toolchain_rss_kB_min	ld	rustc	cargo	slots_held	slots_total	gated	queue_depth	ungated_ld	cpu_some	io_some	io_full	mem_some	disk_avail_kB'

# APPENDING UNDER A DIFFERENT HEADER WOULD SILENTLY MISATTRIBUTE EVERY COLUMN,
# so refuse instead. This is not hypothetical caution: the column set has
# already changed once (queue_depth's meaning, and two columns added), and a
# resumed run pointed at a file from before that change would line 19 values up
# under 17 names and read as plausible data. Refusing costs one flag; a
# mis-columned sample is a wrong number presented as a right one, which is the
# whole thing this protocol exists to prevent.
if [ -e "$out" ]; then
    existing=$(head -n 1 "$out" 2>/dev/null)
    if [ "$existing" != "$(printf '%b' "$header")" ]; then
        echo "sample-box.sh: $out already exists with a DIFFERENT header." >&2
        echo "  Appending would put these columns under the wrong names." >&2
        echo "  Use a fresh --out, or move the old file aside." >&2
        exit 2
    fi
else
    printf '%b\n' "$header" > "$out"
fi

have_fuser=no
if command -v fuser >/dev/null 2>&1; then
    have_fuser=yes
else
    echo "sample-box.sh: WARNING: fuser not found — slots_held will read 0, so" >&2
    echo "  queue_depth over-reports (it becomes the gated total) and ungated_ld" >&2
    echo "  becomes the raw ld count. Install psmisc, or read the 'gated' column" >&2
    echo "  instead; it needs no fuser. Every other column is unaffected." >&2
fi

echo "sample-box.sh: sampling every ${interval}s into $out (Ctrl-C to stop)" >&2

trap 'echo "sample-box.sh: stopped" >&2; exit 0' INT TERM

while :; do
    epoch=$(date -u +%s)
    iso=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    load1=$(awk '{print $1}' /proc/loadavg 2>/dev/null || echo NA)
    memavail=$(awk '/^MemAvailable:/{print $2; exit}' /proc/meminfo 2>/dev/null || echo NA)
    rss=$(toolchain_rss_kb)
    nld=$(count ld); nrustc=$(count rustc); ncargo=$(count cargo)
    gated=$(gate_procs link)

    slots_total=0; slots_held=0
    if [ -d "$pool" ]; then
        for slot in "$pool"/slot.*; do
            [ -e "$slot" ] || continue
            slots_total=$((slots_total + 1))
            # A held slot is one some process has open. flock(2) locks are
            # released by the kernel on exit, so an fd here means a live holder.
            if [ "$have_fuser" = yes ] && fuser "$slot" >/dev/null 2>&1; then
                slots_held=$((slots_held + 1))
            fi
        done
    fi

    # Gate processes beyond the slots that are held are waiting for one. This
    # counts a pre-`ld` waiter, which `nld - slots_held` cannot -- see
    # gate_procs above. Still not exact: a gate process is counted from the
    # moment it starts, so one sampled in the microseconds before it takes a
    # free slot reads as queued for that one sample.
    queue=$((gated - slots_held)); [ "$queue" -lt 0 ] && queue=0

    # What `nld - slots_held` actually measures: linkers running WITHOUT a slot.
    # That is not a queue -- it is the degradation ladder being used, e.g.
    # DAD_LINK_JOBS=0, a missing flock, or the 900s wait budget expiring. Kept
    # as its own column because it is worth seeing, under a name that says what
    # it is. `ld`/`collect2` nesting still makes it indicative, not exact.
    ungated=$((nld - slots_held)); [ "$ungated" -lt 0 ] && ungated=0

    disk=$(df -Pk . 2>/dev/null | awk 'NR==2{print $4}' || echo NA)

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$epoch" "$iso" "${label:-none}" "$load1" "$memavail" "$rss" \
        "$nld" "$nrustc" "$ncargo" "$slots_held" "$slots_total" \
        "$gated" "$queue" "$ungated" \
        "$(psi cpu some)" "$(psi io some)" "$(psi io full)" "$(psi memory some)" \
        "$disk" >> "$out"

    sleep "$interval"
done
