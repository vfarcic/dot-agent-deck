sleep 10
printf '%s\n' '{"type":"turn.started"}'
sleep 2
printf '%s\n' '{"type":"item.started","item":{"type":"command_execution","command":"ls"}}'
sleep 2
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}'
sleep 30
