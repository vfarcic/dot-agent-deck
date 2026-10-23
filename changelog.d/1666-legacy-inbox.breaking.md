## An older `dot-agent-deck` can no longer take deck messages, and `get-seed` is gone

Every agent the deck supports now takes its messages on message receipts, so the deck stops serving the old way of taking them, which an older `dot-agent-deck` still uses. Against this deck daemon an older `dot-agent-deck inbox fetch`, an older agent hook, and an older `inbox wait` from a Codex or Devin hook take nothing: the messages stay waiting and the command or hook says it was refused. Where the agent's hooks run the older copy, the agent's card says its hooks are too old for its messages and names the `hooks install` command that updates them, then restart the agent. Where the model runs an older `inbox fetch`, it says `the deck refused to fetch messages` and that this `dot-agent-deck` is older than the deck daemon: run `inbox fetch` with the `dot-agent-deck` the daemon runs, or upgrade it.

`dot-agent-deck get-seed` is removed. A pane's first prompt has arrived as its first deck message since the deck's messages shipped, and the bundled Pi extension no longer asks for it. An older copy that still asks gets no seed and takes nothing.

`dot-agent-deck ack <id>` of a message the deck has not handed to the agent yet now answers that the id is unknown and changes nothing, instead of marking the message delivered.

This changes which requests the daemon accepts, so a desktop app from this release refuses an older daemon and says which versions they are. Upgrade the deck, the desktop app and the daemon together.
