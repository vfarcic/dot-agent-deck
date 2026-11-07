## `config set` no longer leaves a half-written `config.toml` for a moment

`dot-agent-deck config set` used to empty `config.toml` and then write it again. An agent the deck started at that moment could read the empty or half-written file and start with default settings, such as the doorbell on after you had turned it off. Now the deck writes the new file beside the old one and swaps it into place, so anything reading `config.toml` sees either the old settings or the new ones. A `config.toml` that is a symlink stays a symlink, and the file keeps its permissions.
