## Telemetry Says Why You Stepped In

Each `deck.intervention` event on an `agent.run` span now says what you did, instead of always `input`. `deck.intervention.kind` is `interrupt` (Ctrl+C or Escape), `answer` (input while the agent's card shows **Needs Input**, or an answer you gave by voice in the desktop app), `paste` (a bracketed paste), `text` (typing, Enter, Backspace) or `navigate` (only arrow keys, mouse clicks or scrolling sent to the agent, and other shortcuts). Keystrokes less than about five seconds apart are still one intervention, and it takes the strongest kind among them, in that order, so you can tell steering from approving prompts from scrolling around, for example `GROUP BY deck.intervention.kind` on the `deck.intervention` events. Only the kind is sent, never what you typed or pasted.

A voice answer in the desktop app is now counted also when the deck passes it to the agent without typing into the pane, so `deck.interventions.count` can be higher than before for runs with such answers. Because of both changes, `deck.telemetry.schema_version` is now `3`.

An unmarked paste and voice dictation count as `text`, and typing after Claude Code's end-of-turn reminder counts by what you typed, not as an answer. See [Telemetry](https://agent-deck.devopstoolkit.ai/docs/telemetry.md).
