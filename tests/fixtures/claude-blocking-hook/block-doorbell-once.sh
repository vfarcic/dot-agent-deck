#!/bin/sh
# Issue #2341 (inbox/claude/053): a project policy hook beside the deck's.
# It blocks the first prompt that carries a deck doorbell line, once, and
# leaves a marker saying it did; every other prompt goes through.
input=$(cat)
case "$input" in
  *"dot-agent-deck: message "*) ;;
  *) exit 0 ;;
esac
marker="$CLAUDE_PROJECT_DIR/doorbell-blocked-once.marker"
[ -e "$marker" ] && exit 0
: > "$marker"
printf '%s\n' '{"decision":"block","reason":"blocked once by the test policy hook"}'
