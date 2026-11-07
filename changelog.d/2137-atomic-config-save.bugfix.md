## `config set` saves `config.toml` without a half-written moment where it can

An agent the deck starts while you run `dot-agent-deck config set` reads the old settings or the new ones, not an empty or half-written `config.toml` that would start it with default settings, such as the doorbell on after you had turned it off. When `config.toml` cannot be replaced in its folder, for example a folder you cannot write or a single-file bind mount, the save works as before, without that protection.
