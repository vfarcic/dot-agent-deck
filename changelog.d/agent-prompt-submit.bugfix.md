## Reliable Prompt Submission to Agent Panes

Prompts written to agent panes now self-submit reliably instead of sitting in the agent's input buffer waiting for a manual Enter. Multi-line prompts are wrapped in bracketed paste so embedded newlines stay as input rather than triggering a premature submit, and a brief delay between the payload and the trailing carriage return makes agent CLIs (Claude Code, opencode) honor it as Enter rather than absorbing it as a newline-in-input.

This affects two flows: pressing Ctrl+G to generate a `.dot-agent-deck.toml` config, and orchestration startup where the orchestrator's bootstrap prompt is injected into its agent pane. Both previously left the prompt un-submitted in some cases — the orchestration path additionally fused the role launch command into the prompt buffer because the role command was being written twice (once when the pane was spawned, once again after resize). The duplicate write has been removed.

Status-bar messages now stay visible for 15 seconds instead of 3, so wrapped error messages such as "Orchestration failed: …" remain readable.
