## Codex Agents Started Together Keep Their Hooks Trusted

Starting several Codex agents at once, such as an orchestration with more than one Codex role, no longer leaves some of them with untrusted hooks. Before, the agents could overwrite each other's hook-trust records in `~/.codex/config.toml`, and an agent whose record was lost stopped reporting its status, tools and prompts to the deck partway through a run. The deck now updates that file one agent at a time. Claude Code's `settings.json` and Devin's `config.json` get the same protection when several decks install hooks at once.

The deck keeps a small, empty lock file beside each of those files (for example `~/.codex/.config.toml.lock`). It is safe to delete when no agent is starting. The deck also removes its own temporary files that an interrupted earlier start left behind in those folders (`.config.toml.tmp.*` and similar), once they are more than an hour old and the process that created them has exited.
