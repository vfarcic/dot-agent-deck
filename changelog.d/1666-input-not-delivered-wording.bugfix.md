## The desktop app says why your typing didn't reach an agent, in the TUI's words

When the deck couldn't deliver what you typed into an agent's terminal, the desktop app printed a raw code such as `terminal input rejected: write-failed` in the tile, while the TUI explained it. Both now say the same thing, in words that describe the agent rather than its internals: `Input not delivered: the agent has exited`, `Input not delivered: the agent's terminal did not take it`, `Input not delivered: this agent has no live terminal`, or `Input not delivered: the agent in this pane was replaced or closed`.

A terminal that takes no input at all now reads the same in both clients too: the desktop tile's notice and the TUI's message when you try to type into the card both say `Terminal input unavailable: this agent has no live terminal, only its history` (or `... has no live terminal` when it has no history either).
