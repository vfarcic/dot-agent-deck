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
printf 'TTY-PROBE-READY\n'
while :; do
    if IFS= read -r line; then
        printf 'INPUT=%s\n' "$line" >> "$record"
    fi
done
