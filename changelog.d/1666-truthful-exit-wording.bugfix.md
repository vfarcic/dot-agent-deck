## A message the agent never took says what ended its pane, not that the agent exited

When an agent's pane ended before it took a message, the card and tile said `not delivered: this agent exited before a deck message reached it`, and an orchestrator was told its worker "exited before acknowledging" the task. A pane ending doesn't prove the agent ended: an agent started in the background can still be running.

Now both clients and the orchestrator's message say what ended: the command started in the agent's pane exited (the orchestrator's message gives its exit code), the deck's connection to the agent's pane ended, or the pane was closed or replaced by a new agent, in which case the task stays with the previous agent and the new one doesn't get it. Before you restart the agent or give its task to another, check whether it is still running.

See [Troubleshooting → A message says not delivered](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#a-message-says-not-delivered).
