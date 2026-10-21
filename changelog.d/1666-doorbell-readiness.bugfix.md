## The deck's doorbell no longer types into an agent that is still starting, or for a message already taken

When a worker restarted for a new task, the deck could type its `message … waiting, run: dot-agent-deck inbox fetch` line into the new Claude Code before it had finished starting, where the line could be lost or land on a start-up screen. The doorbell now waits until the new agent shows it is ready for input: until it ends a turn, or, for Claude Code, until its input box is on screen. Codex still gets the line as soon as its empty input box is on screen.

The deck also stops typing the line, and does not press Enter after it, once the message it names is fetched or acknowledged, even while other messages are still waiting, and when you start typing in the pane while it is typing.
