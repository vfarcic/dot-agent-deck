## Undeliverable notices ask the orchestrator to check the worker before giving the task out again

When the deck gave up delivering a delegated task because its worker exited or because the task had been handed over as many times as the deck allows, the orchestrator was told the worker did not receive it and to delegate or reassign it, though the worker could already have it. Following that could get the task done twice. The report now says the worker may have the task and asks the orchestrator to check the worker's pane before giving the task out again, or to tell you. For a task handed over as many times as the deck allows, it also says the worker's `work-done` can still arrive. A task you cancelled before it reached the worker is still reported as not received.

See [Idle workers and notifications](https://agent-deck.devopstoolkit.ai/docs/idle-workers-and-notifications.md).
