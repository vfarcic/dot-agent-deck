## Telemetry Now Sends the Branch Name

**With telemetry on, the deck now sends the name of the git branch each agent starts on**, which it never sent before. Each `agent.run` span carries it as `vcs.ref.head.name`, for example `agent/dispatch-fix-login` for a unit started by `dot-agent-deck dispatch`, so you can join a run to its pull request by branch, including runs that ended before the pull request was opened. The value is absent outside a git repository and on a detached `HEAD`.

A branch name is sent exactly as it was written, so it can carry private text, such as a customer's name, an unreleased product or a security fix. If your branch names should not reach your telemetry backend, delete `vcs.ref.head.name` in an OpenTelemetry Collector on the way; the Collector example in the docs now includes it. See [What is never sent](https://agent-deck.devopstoolkit.ai/docs/telemetry.md) and "Trim what is sent" on the same page.
