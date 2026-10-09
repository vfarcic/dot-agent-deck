#!/bin/sh
# Issue #1493: an interactive-Codex stand-in that walks one turn through
# Codex's NATIVE hooks, one step per `go-<step>` file the test creates, and
# keeps painting TUI-shaped redraw lines the way the real TUI does — on boot,
# and again after its turn has ended. Nothing it prints names a card status.
hook() {
    printf '%s\n' "$1" | dot-agent-deck hook --agent codex
}
step() {
    while [ ! -f "go-$1" ]; do sleep 0.1; done
}
paint() {
    i=0
    while [ "$i" -lt 5 ]; do
        printf '%s\n' '> Ask Codex to do anything' '  gpt-test low  ~/work'
        sleep 0.2
        i=$((i + 1))
    done
}
session='"session_id":"codex-status-standin"'
cwd="\"cwd\":\"$PWD\""

# Like the real TUI, take the terminal out of cooked mode BEFORE painting, and
# give the wrapper time to see it. The wrapper announces its interface ready
# (which the card reads as Idle) and stops watching for output to settle, so
# from then on only what follows can move the card — the boot paint included.
stty -icanon -echo
sleep 1

paint
echo booted > booted.log

step prompt
# Real Codex posts `SessionStart` when its first turn starts, then the prompt.
hook "{$session,\"hook_event_name\":\"SessionStart\",$cwd}"
hook "{$session,\"hook_event_name\":\"UserPromptSubmit\",$cwd,\"prompt\":\"list the files\"}"
printf '%s\n' '* considering the request'

step tool
hook "{$session,\"hook_event_name\":\"PreToolUse\",$cwd,\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls sentinel_dir\"}}"
printf '%s\n' '* Running ls sentinel_dir'

step permission
hook "{$session,\"hook_event_name\":\"PermissionRequest\",$cwd,\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls sentinel_dir\"}}"
printf '%s\n' '  Allow command? [y/n]'

step tooldone
hook "{$session,\"hook_event_name\":\"PostToolUse\",$cwd,\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls sentinel_dir\"},\"tool_response\":\"\"}"

step stop
hook "{$session,\"hook_event_name\":\"Stop\",$cwd}"
echo stopped > stopped.log

# The TUI redraws after a turn ends; none of that is work.
paint
echo redrawn > redrawn.log
sleep 600
