## A `config.toml` link to a missing file holds the doorbell off

When the deck's `config.toml` is a link to a file that is no longer there, for example one that was moved or is on a drive that isn't mounted, the deck now treats it as a config it can't use: it holds the doorbell off for both Claude Code and Codex for the agents it starts, and the deck log names the file, instead of turning the doorbell on as if there were no config at all. `DOT_AGENT_DECK_DOORBELL_CLAUDE` and `DOT_AGENT_DECK_DOORBELL_CODEX` still win over it.
