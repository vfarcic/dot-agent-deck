## A message the agent never took says what ended its pane, not that the agent exited

When an agent's pane ended before it took a message, the card and tile said `not delivered: this agent exited before a deck message reached it`, and an orchestrator was told its worker "exited before acknowledging" the task. The deck can't see the agent itself, only the command it started in the pane and that pane's terminal, so that could be wrong: an agent started in the background by that command can still be running after it exits.

Now both clients and the orchestrator's message say what the deck saw: the command started in the agent's pane exited (the orchestrator's message gives its exit code), the deck's connection to the agent's pane ended and whether the agent is still running is unknown, or the pane was closed or replaced by a new agent, in which case the task stays with the previous agent and the new one doesn't get it.

See [Troubleshooting → A message says not delivered](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#a-message-says-not-delivered).
