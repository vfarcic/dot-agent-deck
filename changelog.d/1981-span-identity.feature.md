## Telemetry: every span names its agent, and CI is counted once per pull request

The deck's exported telemetry is easier to slice and no longer counts the same CI twice. Every `agent.status`, `agent.operation` and `agent.ci` span, and every event on a run (`deck.status_change`, `deck.intervention`, `deck.ci.change`, `deck.model_usage`), now carries `deck.agent`, and `deck.role` and `deck.orchestration.id` for an orchestration role, so you can group them by agent or role without joining them to their run.

A run started through a launcher the deck does not recognise, such as `devbox run …` or a plain shell where you then start an agent, now takes the agent its hooks report, instead of staying `none`. A new `deck.agent.source` attribute says whether the agent was known when the run started (`spawn`) or learned from its hooks (`hook`). An agent the deck knew at start stays what it was.

When several runs watch the same pull request, as the roles of one orchestration do, its `agent.ci` spans and `deck.ci.change` events are now sent once rather than once per run. `deck.ci.rounds.count` and `deck.ci.failures.count` stay on each run and count what that run saw, so sum them per pull request, not per run. `deck.pr.number` on a run now keeps the last pull request the run saw, even if its branch later has none, and a run started by `dot-agent-deck dispatch` carries the unit's name in `deck.dispatch.name`.

See [Telemetry](https://agent-deck.devopstoolkit.ai/docs/telemetry.md) for the full list of attributes.
