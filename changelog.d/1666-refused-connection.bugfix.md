## An agent whose own requests for its messages are refused says so, instead of its messages queuing silently

When the deck refused an agent's own request for its deck messages, the messages stayed queued and nothing on the agent's card or tile said why. Now, while messages wait for that agent, the TUI card and the desktop tile both say `Messages can't reach this agent`, give the cause (for example, that the deck could not identify the process that asked for them), and tell you to restart the agent from the deck. Messages addressed to the agent are not handed to the restarted one, so have them sent again; an orchestrator whose delegated task this was is told it was not delivered. The notice clears once one of the agent's requests is accepted, or when the agent exits. In the TUI, press `i` on the card to read the whole notice when the card is too narrow to show it. An orchestrator whose delegated tasks are waiting for that agent when the refusal starts gets one deck message saying the worker cannot take its messages, naming them.

The command that was refused also prints the cause and what to do. A request that names another agent's pane never marks that agent's card.

See [Troubleshooting → An inbox request is refused](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#an-inbox-request-is-refused).
