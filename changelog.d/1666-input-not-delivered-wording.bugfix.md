## The desktop app says why your typing didn't reach an agent, in the TUI's words

When the deck couldn't deliver what you typed into an agent's terminal, the desktop app printed a raw code such as `terminal input rejected: write-failed` in the tile, while the TUI explained it. Both now say the same thing: `Input not delivered: the agent has exited`, `Input not delivered: the agent's terminal did not take it`, `Input not delivered: session is history-only`, or `Input not delivered: the pane's session changed`.
