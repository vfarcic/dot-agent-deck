# Delegate delivery

Delegated tasks now use the per-agent inbox described in [Agent messaging](agent-messaging.md). A task is queued for the selected worker generation, fetched by its integration and acknowledged on that agent's receipt signal. The task-file pointer can be inside the message body; it is not typed into the worker's composer.

The inbox id replaces the old delivery id for acknowledgement and dedup. A hook event from an unrelated turn does not settle pending work. Redelivery preserves the message id, uses the inbox ack-wait and delivery limit, and leaves the worker process running. A recipient exit makes its bound messages undeliverable rather than moving them silently to a successor.

The former echo-gated Enter, screen-based probe → grace → retype loop and provisional prompt confirmation are removed. Human typing and the guarded fixed doorbell remain PTY input. The doorbell guards and per-agent acknowledgement table live in the new guide.
