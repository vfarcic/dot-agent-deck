# Delegate delivery

Delegated tasks now use the per-agent inbox described in [Agent messaging](agent-messaging.md). A task is queued for the selected worker generation, fetched by its integration and acknowledged on that agent's receipt signal. The task-file pointer can be inside the message body; it is not typed into the worker's composer.

The inbox id replaces the old delivery id for acknowledgement and dedup. A hook event from an unrelated turn does not settle pending work. Redelivery preserves the message id, follows the receipt attempt model's possession, lapse and retry rules, and leaves the worker process running. The attempt model still uses `InboxLimits::ack_wait` and `max_deliver`, but a fetched message has no store-level ack-wait deadline; see [Receipt timing and retry bounds](agent-messaging.md#receipt-timing-and-retry-bounds). A recipient exit makes its bound messages undeliverable rather than moving them silently to a successor.

Which delegated task a report completes is no longer inferred: `work-done --task-id <id>` names it, `--done` is a terminal report and cannot be combined with `--task-id`, and a `work-done` with neither is a note to the orchestrator that completes no task. See [Task-scoped completion](agent-messaging.md#task-scoped-completion-prd-1666-part-3-round-5).

The former echo-gated Enter, screen-based probe → grace → retype loop and provisional prompt confirmation are removed. Human typing and the guarded fixed doorbell remain PTY input. The doorbell guards and per-agent acknowledgement table live in the new guide.
