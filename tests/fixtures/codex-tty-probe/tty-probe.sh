#!/bin/sh

record=tty-probe.log
: > "$record"
for fd in 0 1 2; do
    if [ -t "$fd" ]; then
        value=true
    else
        value=false
    fi
    printf 'isatty(%s)=%s\n' "$fd" "$value" >> "$record"
done

trap 'printf "WINCH\n" >> "$record"' WINCH
trap 'printf "INT\n" >> "$record"; exit 0' INT
# Written only once both traps are installed: a SIGWINCH or SIGINT that lands
# before them is not recorded, so the test waits for this line rather than for the
# isatty lines above.
printf 'TRAPS-READY\n' >> "$record"
printf 'TTY-PROBE-READY\n'
while :; do
    if IFS= read -r line; then
        printf 'INPUT=%s\n' "$line" >> "$record"
    fi
done
