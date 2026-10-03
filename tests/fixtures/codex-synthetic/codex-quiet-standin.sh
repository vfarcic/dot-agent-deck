#!/bin/sh
# Issue #1493: a Codex stand-in for a pane whose hooks the deck could not get
# trusted. It reports nothing through any hook — its output is all the deck can
# see — and paints in bursts, one per `go-paint-<n>` file the test creates.
paint() {
    i=0
    while [ "$i" -lt 8 ]; do
        printf '%s\n' '> Ask Codex to do anything' '  gpt-test low  ~/work'
        sleep 0.25
        i=$((i + 1))
    done
}
paint
echo booted > booted.log
while [ ! -f go-paint-1 ]; do sleep 0.1; done
paint
echo painted > painted.log
sleep 600
