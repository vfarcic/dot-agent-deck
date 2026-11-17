# Agent Cards

Agent cards are an experimental view of the desktop app's Dashboard. Each agent the daemon runs gets a card that says what it is doing, whether it needs you, what it last reported about its work, and the actions you can take on it, such as answering its question or telling it to go ahead. You read many agents at a glance and act on the one that needs you without opening its terminal.

The cards are hidden unless the `experimental` flag is on. See [Turning the cards on](#turning-the-cards-on).

## What a card shows

A card holds two kinds of information, and the app labels each so you can tell them apart:

- **What the deck checked.** The agent's status (Working, Waiting for input, Idle, Stopped and the rest), which dispatcher or orchestration it belongs to, and the pull request of its branch. The deck works these out itself. The overlay marks them **deck checked**.
- **What the agent says.** A short summary, its own status (working, waiting, done or failed), named fields such as its progress or the pull request it opened, questions for you, and actions it suggests. These are the agent's own words and the deck does not check them. They are marked **agent says**.

An agent that has reported nothing yet still has a card, with its deck-checked status and the deck's own actions, and the words "No report from this agent yet."

A card is marked **Needs you** when its agent stopped, is asking a question in its terminal or on its card, says it is waiting for you, done or failed, or is blocked. Hover the mark to see which.

## Switch between the table and the cards

With the flag on, the Dashboard's top bar has a **Table | Cards** switch. **Table** is the Dashboard you know. **Cards** shows a grid of cards. The app remembers your choice in this window's storage, so the next time you open the Dashboard it shows the same view.

In the Cards view:

- Cards are grouped as the daemon groups them: the agents a dispatcher started are one group, with the dispatcher's own card in it; the roles of an orchestration are another; agents in neither are grouped together as standalone. Each group's heading counts its agents and how many need you.
- Within a group, the cards that need you come first. The order is the same everywhere you look at the cards, including the next and previous moves in the card overlay.
- **Needs you** above the grid shows only the cards that need you. Press it again to see every card.
- Each card has one main button, the action the agent most needs from you: the answer to its question, an action it suggested, or **Open** when it needs nothing. Under the button is the start of the exact text it sends, and hovering shows all of it. After you press it, the card says whether the message was sent and, later, whether the agent took it.
- With several daemons on the Dashboard, each daemon's cards are under its own name. A daemon whose version does not have cards keeps its usual table section.

In the Table view, a row whose card needs you shows a bell next to the agent's name, and the row's **⋯** menu has **Show card**, which opens that agent's card, and the card's main action. Clicking the row itself still opens the agent's terminal, as before.

## The card overlay

Click a card's name, or **Show card** in a row's menu, to open the card overlay. It shows everything on the card: the deck's status and pull request, the agent's report and every field, each question with its answers as buttons, and every action with the exact text it sends to the agent, why it is disabled when it is, and what happened to the last time you pressed it.

- **[** and **]**, or the arrow buttons, move to the previous and next card without closing the overlay. **n**, or **Next needing you**, jumps to the next card that needs you. **Escape** or the **×** closes it.
- The overlay updates while it is open: when the agent publishes a new report or a message is delivered, you see it straight away. If the agent is closed while you look at its card, the overlay says "This agent was closed." and you can move on to the next card.
- An action marked **Asks you to confirm first**, and both ways of closing the agent, show you exactly what will be sent or done and ask before doing it. Closing tells you what is still pending, such as an unanswered question or an open pull request.
- **Reply** opens a box where you write your own message. It is sent to the agent exactly as you typed it.
- **Open** opens the agent's terminal pane. **Close** closes the agent, and **Close, keep worktree** closes a dispatched agent but keeps its copy of the repository, when it has one.
- Actions that send the agent a message are disabled while the agent is not running, and the overlay says so.

An action is always sent to the agent whose card it is on, and only to that agent. If the agent was replaced (its pane restarted) after you opened its card, the action is refused rather than sent to the new one.

## By voice

With voice control on and the cards shown, you can say:

| Say | What happens |
| --- | --- |
| "show cards", "show the table" | Switches the Dashboard's view. |
| "show the coder's card" | Opens that agent's card. |
| "next card", "previous card" | Moves the open card overlay. |
| "next one that needs me" | Jumps to the next card that needs you. |
| "only the ones that need me", "show all cards" | Turns the **Needs you** filter on or off. |
| "close the card" | Closes the card overlay. |
| "press go ahead on the coder's card" | Presses that action, named by the label on its button. |
| "reply to the coder use the main branch" | Sends your words to that agent as a reply. |

An action that asks you to confirm, and closing an agent, are never done by voice alone: the card opens at its confirm step and you press the button yourself.

## Which agents publish a card

An agent publishes its card by running `dot-agent-deck card publish` in its pane, so any supported agent that can run a command (Claude Code, Codex, OpenCode, Pi and Devin) can publish one. An agent kept by its own sandbox from running `dot-agent-deck` cannot, and its card shows only what the deck checked.

When the deck dispatches work, the task it gives the agent explains how to keep its card current and lists the fields and actions it can use. An agent you start yourself is not told: ask it to run `dot-agent-deck card publish --help` and `dot-agent-deck card catalog`. Publishing a card notifies nobody; it is not the agent's report that its task is done.

## Choose the fields and actions agents use

The fields agents fill in and the actions they can offer you come from a catalog. The deck has defaults: the fields `progress`, `next-step`, `details`, `pull-request`, `issue` and `links`, and the actions **Go ahead**, **Merge it** and **Explain**. `dot-agent-deck card catalog` prints the catalog the daemon uses.

Change it with a `[catalog]` table in your user configuration file, `~/.config/dot-agent-deck/config.toml` (or the file `DOT_AGENT_DECK_CONFIG` names). The daemon reads that file, so for a daemon on another machine it is the file on that machine. Edits apply to the next card read or publish, with no restart.

```toml
[catalog]
remove = ["merge"]                 # drop default fields or actions by name

[[catalog.fields]]
name = "risk"                      # lower case letters, digits and dashes
description = "How risky the change is: low, medium or high."
type = "text"                      # text, long_text, number, boolean, url, list, pull_request or issue
label = "Risk"                     # optional
icon = "alert-triangle"            # optional
placement = "card"                 # optional: compact, card (the default) or detail

[[catalog.actions]]
name = "ship"
description = "Offer when the work is ready to release."
prompt = "Release it following this project's procedure, then update your card."
label = "Ship it"                  # optional
placement = "card"                 # optional
confirm = true                     # optional: ask before sending
always = false                     # optional: true offers it on every card, suggested or not
```

- A field or action with the same `name` as a default replaces it whole; a new name is added after the defaults.
- `description` tells agents when to use the entry. `prompt` is exactly what the action sends to the agent.
- `placement = "card"` and `"compact"` show a field on the card and in the overlay; `"detail"` only in the overlay.
- An entry with a mistake (an unknown type, a missing description, a bad name) is skipped and the rest still apply. `dot-agent-deck card catalog` prints a warning for each one.
- The actions **Reply**, **Open**, **Close** and **Close, keep worktree** are the deck's own and cannot be removed or changed.
- A `[catalog]` table in a project's `.dot-agent-deck.toml` is not used yet. Only the user configuration file's applies.

Agents can also report fields and actions that are not in the catalog. You see them marked **custom**; a custom field shows only in the overlay.

## Turning the cards on

The cards are behind the `experimental` flag. Start the desktop app with `DOT_AGENT_DECK_EXPERIMENTAL=1` in its environment, as described in [Features behind the `experimental` flag](index.md#features-behind-the-experimental-flag). The daemon needs no flag: it keeps every agent's card either way, and the `dot-agent-deck card` commands work without it. A daemon from a release before cards were added has no cards, and the app shows its agents in the table.

## In the TUI

The TUI does not show cards. Its dashboard keeps showing each agent's status as before. From any shell on the machine, `dot-agent-deck card list` prints every card as JSON, with the same status, needs-you reasons and actions the app shows, and `dot-agent-deck card act --entry <pane>/<agent> --action <action>` presses one action, adding `--confirm` for one that asks for confirmation and `--text` for **Reply**. Run `card act` from your own shell, not from an agent's pane: the deck refuses it there, so that one agent does not press another agent's buttons by mistake.
