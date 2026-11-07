## `config set` no longer leaves a half-written `config.toml` for a moment

An agent the deck starts while `dot-agent-deck config set` is saving can no longer read an empty or half-written `config.toml` and start with default settings, such as the doorbell on after you had turned it off: it sees either the old settings or the new ones. When the folder that holds `config.toml` is not writable, or the file is mounted on its own (a single-file bind mount), the save works as before, without that protection.
