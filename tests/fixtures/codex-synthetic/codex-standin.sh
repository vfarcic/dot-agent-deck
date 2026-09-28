sleep 10
# What the interactive `codex` TUI actually paints: redraw text, never a
# `"type":…` JSON record (issue #540). The wrapper reads each line as activity.
printf '%s\n' '› Ask Codex to do anything'
sleep 2
printf '%s\n' '• Running ls'
if IFS= read -r line; then
    printf '%s\n' "$line" > managed-wrapper-input.log
fi
sleep 2
# A turn ends the way it does for real Codex: through its NATIVE `Stop` hook,
# which the deck's hooks.json points at `dot-agent-deck hook --agent codex`.
# Nothing on stdout can say it, because the TUI prints nothing that means it.
printf '%s\n' "{\"session_id\":\"codex-standin\",\"hook_event_name\":\"Stop\",\"cwd\":\"$PWD\"}" | dot-agent-deck hook --agent codex
sleep 30
