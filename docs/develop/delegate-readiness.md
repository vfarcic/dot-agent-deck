# Delegate readiness

Tasks for a respawned worker now wait in its inbox. See [Agent messaging](agent-messaging.md) for the consumer lifecycle, native announcement grace and guarded doorbell, and [Orchestration](../orchestration.md#what-clear-does-to-delivery) for user behavior.

A `clear = true` delegation replaces the agent in the role's pane and binds the new task to that generation. A `clear = false` delegation queues work for the existing generation. Native wake depends on an authenticated agent-side component arming, not the launch command or a guessed readiness delay. A component that announces native wake receives an arm grace before fallback ringing; the doorbell itself still requires idle status and a clear input box.

The former readiness buffers, launcher delay and `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` override are removed. Agent identification is still relevant to status, UI and agent-specific doorbell guards; it does not authorize typing a task body.
