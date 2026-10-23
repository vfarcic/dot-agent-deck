## Codex, Devin and `inbox fetch` confirm each deck message they take, and must match the deck's release

Codex's deck hooks and `dot-agent-deck inbox fetch` now tell the deck exactly which hand-over of a message they printed, as Claude Code's hooks already do. A message one of them did not print in full is sent again, at most twice and marked as a possible repeat, and if that still does not settle it the TUI card and the desktop tile show `✉ delivery uncertain` with **Retry** and **Cancel**. This covers Codex with its hooks trusted, Codex with its prompt hook switched off or its hooks untrusted (the model runs `inbox fetch` when the doorbell asks it to), Devin, and any other agent whose model runs `inbox fetch`.

Codex's hooks, `inbox fetch` and the deck daemon must come from the same release. Against an older daemon they take no message and say why:

- **Codex's hooks newer than the daemon.** The hook's error says the daemon did not answer in time, or did not confirm that it serves message receipts, and the messages wait. Restart the daemon on the hooks' release, or run `dot-agent-deck hooks install --agent codex` with the release the daemon runs, then restart the agent.
- **`inbox fetch` newer than the daemon.** It prints `Could not fetch messages: the deck daemon did not confirm that it can hand them to this dot-agent-deck inbox fetch` (or that it did not answer in time), takes nothing, and the messages wait. Restart the deck daemon on this release, or run `inbox fetch` with the `dot-agent-deck` the daemon runs.

`inbox fetch --json` prints each message's id, subject, sender and text, as before. Each message block that `inbox fetch` and Codex's hooks print now names its hand-over in its first line (`[dot-agent-deck message <id> delivery 1.1 from …]`).

See [Troubleshooting → Codex's deck hook or `inbox fetch` shows an error](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#codexs-deck-hook-or-inbox-fetch-shows-an-error).
