## A worker's response time starts when it receives its task

The `A delegated worker has not responded with work-done` report could reach the orchestrator early when the deck took a while to hand a task over, for example while it restarted a `clear = true` worker: that time was counted against the worker. Now the worker gets the whole `worker_response_timeout_minutes` from the moment it receives its task.

See [Idle Workers & Notifications](https://agent-deck.devopstoolkit.ai/docs/idle-workers-and-notifications.md).
