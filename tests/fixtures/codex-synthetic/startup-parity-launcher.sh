i=0
while [ ! -f "$HOME/.codex/config.toml" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
if [ -f "$HOME/.codex/config.toml" ]; then
    printf '%s\n' "{\"session_id\":\"startup-codex-parity\",\"hook_event_name\":\"UserPromptSubmit\",\"cwd\":\"$PWD\",\"prompt\":\"STARTUP_CODEX_PARITY\"}" | dot-agent-deck hook --agent codex
fi
sleep 30
