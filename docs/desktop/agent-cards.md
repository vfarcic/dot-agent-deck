# Agent Cards

Agent cards are a view of the desktop app's Dashboard. Each agent the daemon runs gets a card that says what it is doing, whether it needs you, which pull requests and issues it reported, and the actions you can take on it, such as answering its question or telling it to go ahead. The cards are laid out as a tree: you see the agents you started, and you open one to see the agents it started. You read many agents at a glance and act on the one that needs you without opening its terminal.

## What a card shows

A card holds two kinds of information, and the app labels each so you can tell them apart:

- **What the deck checked.** The agent's status (Working, Needs input, Idle, Stopped and the rest), which agent started it, the pull request of its branch, and the pull requests and issues the agent reported, looked up on GitHub by the deck. The deck works these out itself. The overlay marks them **deck checked**.
- **What the agent says.** A short summary, its own status (working, waiting, done or failed), named fields such as its progress or the pull request it opened, questions for you, and actions it suggests. These are the agent's own words and the deck does not check them. They are marked **agent says**.

An agent that has reported nothing yet still has a card, with its deck-checked status and the deck's own actions, and the words "No report from this agent yet." in its overlay. An agent that clears its card (`dot-agent-deck card publish --clear`) goes back to that.

A card is marked **Needs you** when its agent stopped, is asking a question in its terminal or on its card, says it is waiting for you, done or failed, or is blocked. Hover the mark to see which.

## Switch between the table and the cards

The Dashboard's top bar has a **Table | Cards** switch. **Table** is the Dashboard you know. **Cards** shows the cards. The app remembers your choice in this window's storage, so the next time you open the Dashboard it shows the same view.

![The Dashboard's Cards view: under the Local daemon's header, two cards side by side, the Plan / architecture orchestrator, whose tree is open, and a standalone agent that needs input; under the orchestrator, joined by a line, its two roles, one of them showing its checked pull request #18421 and its main button, “Yes, behind a flag” shortened to fit, marked **agent says**](/img/agent-cards-desktop.png)

## The tree of cards

The Cards view starts with the agents nothing else started: a dispatcher, an orchestration you started yourself (its orchestrator's card), and an agent you started on its own. The agents they started are under them:

- A dispatcher's card has the agents it dispatched under it. When it dispatched an orchestration, that orchestration's orchestrator is under the dispatcher, and its roles are under the orchestrator.
- An orchestrator's card has the orchestration's roles under it.
- A card whose starter was closed moves up to the top row.

Press the arrow on a card to open it and see the agents under it, and press it again to close it. A card under another one opens the same way, as deep as the agents go. Only a card with agents under it has the arrow.

The cards in the top row stay where they are. They fill a row and wrap to the next, in the order their agents started, and they do not move when a status changes, when an agent comes to need you, or when you open one. An open card's tree is drawn below its row, joined to it by a line; when you open more than one card in a row, each tree gets its own level below that row, in the cards' order, and the rows below move down to make room. A tree wider than the window scrolls sideways on its own, without moving the rest of the page.

A closed card with agents under it tells you about them:

- **Its colour** is the most urgent status among the card and every agent under it, so a dispatcher whose unit needs input is coloured as needing input. The order, most urgent first: Needs input, Blocked, Error, Waiting, Working, Thinking, Compacting, Idle, Stopped.
- **A bar** shows how many agents under it are in each status, in the same colours.
- **A line** such as "4 under it · 1 needs you" counts every agent under it, at every depth, and how many of them need you.

These update while you watch. An open card keeps its bar and line and takes its own status's colour, since the agents under it are on screen.

The app remembers which cards you opened, for each daemon, in this window's storage, so the next time you open the Dashboard the same trees are open, as long as that daemon has not been restarted.

### Needs you only

**Needs you only**, above the cards, opens the way to every card that needs you and fades every card that neither needs you nor leads to one. Press it again to show every card as usual.

While it stays on, a card that comes to need you is opened up to as well. A tree you close stays closed until an agent under it newly needs you, and then it opens again.

### The dashboard's filter

The Dashboard's filter applies to the cards as it does to the table rows: with a status, kind, agent type, daemon or search text chosen, only the cards of the agents it shows are on screen. A card whose starter the filter hides is drawn in the top row until you clear the filter.

### A daemon from an older release

A daemon from a release before the tree was added sends no tree: the app shows its cards side by side, without the bar, the counts or checked pull requests and issues, and says "This daemon is older than the app, so its cards are shown side by side rather than as a tree." Update dot-agent-deck on the machine that daemon runs on to get the tree. A daemon from a release before cards were added has no cards and keeps its usual table.

## What is on a card

Each card has fixed-size lines, so cards line up and stay the same size whatever their agents report. A line too long for the card is cut short, and hovering it shows all of it.

- **The agent's name.** Click it to open the card overlay.
- **What it is** — dispatcher, orchestrator, role or agent — then its status in the same words as the table and the TUI, and its agent type, such as Claude Code or Codex.
- **Needs you**, when it does.
- **The pull requests and issues the agent reported, as the deck checked them** (see [Pull requests and issues on a card](#pull-requests-and-issues-on-a-card)). When there are more than fit, **+N** at the end of the line counts the rest; press it to open the card's overlay at its list of every one.
- **The bar and count** of the agents under it, on a card that has any.
- **One main button**, the action the agent most needs from you: the answer to its question or an action it suggested. When there is nothing for you to answer or press, the card has no button; click its name to open it. Under the button is the start of the exact text it sends, and hovering shows all of it. An action the agent wrote itself is marked **agent says**. The button sends straight away only an action whose text comes from your catalog and that does not ask you to confirm first. For anything else, such as an action the agent wrote, or an answer to its question, the button sends nothing yet: it opens the card at a step showing the whole text, and you send it from there. After you send it, the card says whether the message was sent and, later, whether the agent took it.

The agent's summary, progress, next step and other fields are not on the card: they are in its overlay.

Each daemon keeps its own section with its header, so its **Stop daemon**, **Upgrade**, host figures and **New agent** buttons are there in both views, and its cards are below the header.

In the Table view, a row whose card needs you shows a bell next to the agent's name, and the row's **⋯** menu has **Show card**, which opens that agent's card, and the card's main action. The menu shows only the action's name, so choosing it sends nothing yet: it opens the card at a step that shows exactly what the action sends, and you send it from there. Clicking the row itself still opens the agent's terminal, as before.

## Pull requests and issues on a card

When an agent reports a pull request or an issue on its card, the deck looks it up on GitHub itself and shows what GitHub says, not what the agent wrote. A pull request shows the same badge as the Dashboard's [pull request badge](dashboard.md#pull-requests): its state, its CI checks and its review. An issue shows its number and whether it is open or closed. Hover a badge to read it in words, with when the deck checked it and what the agent wrote. The deck checks them again while they can still change, and stops once a pull request is merged or closed with its checks finished, or an issue is closed.

Click a badge to open GitHub's page for it inside the app, as the Dashboard's pull request badge does (see [Read a pull request in the app](dashboard.md#read-a-pull-request-in-the-app)). The card overlay closes so that the page is in front. This works on a stopped agent's card as well, for as long as the daemon lists that card. If the page cannot be opened, the app says why, on the overlay or above the cards: for example, "That card is no longer on its daemon, so its page was not opened."

What the deck reads, and how:

- It reads the `pull-request` and `issue` fields, and any other field of type `pull_request` or `issue`, whether the catalog defines it or the agent reports it as a custom field. A number such as `#123` in the agent's summary or other text is not read: ask the agent to put it in one of those fields.
- It reads `#123`, `owner/repo#123` and a github.com address such as `https://github.com/owner/repo/pull/123` or `https://github.com/owner/repo/issues/123`. A plain `#123` means that number in the repository the agent is working in, the GitHub `origin` of its folder. An address says for itself whether it is a pull request or an issue, so a pull request address in the `issue` field is shown as the pull request it is.
- It looks them up with the `gh` command on the machine the daemon runs on, with that machine's GitHub login. That is the machine that needs `gh` installed and logged in, not the one you run the app on.

Until the first lookup finishes, the badge reads **checking…**. A badge marked **stale** shows what the deck found last time, because its latest check failed; hover it to see when that was. When the deck could not check one, the card shows why in a few words, with the full sentence when you hover it and in the overlay:

| The card shows | What it means | What to do |
| --- | --- | --- |
| no GitHub access | `gh` is missing or not logged in on the deck's host. | Install `gh` on the machine the daemon runs on and run `gh auth login` there. |
| not found or private | GitHub has nothing by that number that the deck's login can see. It may not exist, or it may be private to a login the deck's host does not use. | Check the number and the repository. For a private repository, log `gh` in on the deck's host with an account that can see it. |
| GitHub rate limit | The deck's host has used up its GitHub requests for now. | Wait: the deck tries again later by itself. |
| check failed | The deck's host could not check it for another reason, such as a repository that was renamed or moved. | Read the deck's log ([Troubleshooting → Enabling Debug Logs](../troubleshooting.md#enabling-debug-logs)). For a renamed repository, ask the agent to report the repository's current name. |
| not a GitHub reference | The agent wrote something the deck cannot read as a GitHub pull request or issue, or a `#123` in a folder with no GitHub `origin`. | Ask the agent to report `#123`, `owner/repo#123` or the github.com address. |

## The card overlay

Click a card's name, or **Show card** in a row's menu, to open the card overlay. It shows everything on the card: the deck's status and the pull request of the agent's branch, the agent's report and every field, each question with its answers as buttons, and every action with the exact text it sends to the agent, why it is disabled when it is, and what happened to the last time you pressed it. **Sent from this card** lists the last actions you sent the agent, each with the exact text and whether it was delivered, and keeps them after the agent's card stops offering those actions.

**Pull requests and issues**, marked **deck checked**, lists every pull request and issue the agent reported, in the order of its fields. Each one has its badge, which opens it as on the card, what the deck found (for example `Issue #3: closed`, or a pull request's state, review and CI) with its title on GitHub, when the deck checked it, that the check may be out of date, that it is still to be checked, or why it cannot be checked, and what the agent wrote, marked **agent says**.

The text an action sends is shown exactly as it is sent. Characters you could not otherwise see, such as a tab, a character that changes the direction of the text around it, or one that joins or alters the characters next to it, are shown as a boxed code between `⟦` and `⟧`, like `⟦tab⟧` or `⟦U+202E⟧`, with a note under the text saying what the codes mean; hover a code to see what it stands for. Nothing is left out. A `⟦` the agent typed is itself shown as the code `⟦U+27E6⟧`, so ordinary text can never pass for one of these codes, and text such as `\t` that the agent typed is shown as the plain characters it is.

- **[** and **]**, or the arrow buttons, move to the previous and next card without closing the overlay, in the tree's order: each agent comes after the one that started it, and cards inside a closed tree are included. **n**, or **Next needing you**, jumps to the next card that needs you. **Escape** or the **×** closes it.
- The overlay updates while it is open: when the agent publishes a new report or a message is delivered, you see it straight away. If the agent is closed while you look at its card, the overlay says "This agent was closed." and you can move on to the next card.
- An action marked **Asks you to confirm first**, and both ways of closing the agent, show you exactly what will be sent or done and ask before doing it. Closing tells you what is still pending, such as an unanswered question or an open pull request.
- If the agent changes an action while you are looking at it, the confirm step shows the new text, says it changed, and sends nothing until you confirm you have read it. A button pressed after the agent changed its action is refused, and nothing is sent; look at the card again and press it once more if you still want it.
- **Reply** opens a box where you write your own message. It is sent to the agent exactly as you typed it.
- **Open** closes the card and opens the agent's terminal pane. **Close** closes the agent, and **Close, keep worktree** closes a dispatched agent but keeps its copy of the repository, when it has one.
- Actions that send the agent a message are disabled while the agent is not running, and the overlay says so.
- What happened to a message you sent reads **Queued for the agent**, **Delivered to the agent**, **Not delivered** with the reason, or **Delivery uncertain** when the deck cannot tell whether the agent got it, or has run out of attempts to send it, or has seen the agent hold it for a long time without starting on it. An uncertain message is also in the agent's inbox, where you can retry or cancel it.

An action is always sent to the agent whose card it is on, and only to that agent, with the text the card showed you. The agent is told that you pressed it on its card; when the text is, or quotes, what the agent itself wrote on the card (an action or answer it worded, or an answer that repeats its question), the agent is told that too, and gets that text marked as untrusted, so it does not take its own words for yours or for a message from the deck. If the agent was replaced (its pane restarted) after you opened its card, or the deck's daemon was restarted, the action is refused rather than sent to whichever agent has its place now.

## By voice

With voice control on, you can say:

| Say | What happens |
| --- | --- |
| "show cards", "show the table" | Switches the Dashboard's view. |
| "show the coder's card" | Opens that agent's card. |
| "open the dispatcher's tree", "close the dispatcher's tree" | Opens or closes the tree under that agent's card. Opening it also opens the way down to that card when it is inside a closed tree. |
| "open the pull request on the coder's card", "open the issue on the coder's card" | Opens, inside the app, the pull request or issue that agent reported, as the deck checked it. |
| "next card", "previous card" | Moves the open card overlay. |
| "next one that needs me" | Jumps to the next card that needs you. |
| "only the ones that need me", "show all cards" | Turns **Needs you only** on or off. |
| "close the card" | Closes the card overlay. |
| "press go ahead on the coder's card", "answer main to the coder" | Presses that action or answer, named by the label on its button. |
| "reply to the coder use the main branch" | Opens that agent's card with your words in its **Reply** box, for you to read, change and send with **Send reply**. |

When the agent reported more than one pull request, or more than one issue, voice opens the first one the deck could check; click a badge to open another. If the dashboard's filter hides the card whose tree you asked to open, the app opens it anyway and tells you to clear the filter to see it.

With **All daemons** selected, you can name an agent on any daemon the Dashboard shows. When the name fits agents on more than one daemon, the app asks which; add the daemon's name, as in "show the coder's card on build box", to pick it straight away (see [Naming an agent](voice.md#naming-an-agent)).

Voice sends an action straight away only when its text comes from your catalog. Anything with text the agent wrote opens the card at a step showing exactly what would be sent, and the voice row says so; press **Send** to send it. That includes an action the agent suggested that is not in your catalog and every answer to one of its questions: an answer the agent gave no text of its own sends `Answer to your question "<question>": <answer>`, which repeats the agent's question and answer words. An action that asks you to confirm, and closing an agent, are never done by voice alone either: the card opens at its confirm step and you press the button yourself. A spoken reply is not sent by voice alone, as dictating to an agent's terminal types your words without pressing Enter: you see what was heard before it goes.

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
- A field of type `pull_request` or `issue` is checked by the deck like the default `pull-request` and `issue` fields (see [Pull requests and issues on a card](#pull-requests-and-issues-on-a-card)).
- The desktop app shows every field in the card overlay, whatever its `placement`; `dot-agent-deck card list` reports each field's placement.
- An entry with a mistake (an unknown type, a missing description, a bad name) is skipped and the rest still apply. `dot-agent-deck card catalog` prints a warning for each one.
- A mistake elsewhere in the file that makes it unreadable, such as a missing quote in another setting, does not cost you the catalog: the deck reads the `[catalog]` tables on their own, uses them, and warns that the file needs fixing. A mistake inside the `[catalog]` tables themselves leaves the defaults in use, with a warning. `dot-agent-deck config set` keeps your `[catalog]` and its comments, and refuses to save over a file it cannot read.
- The actions **Reply**, **Open**, **Close** and **Close, keep worktree** are the deck's own and cannot be removed or changed.
- A `[catalog]` table in a project's `.dot-agent-deck.toml` is not used yet. Only the user configuration file's applies.
- Changes apply to cards agents already published. When you remove an action from the catalog, it disappears from the cards that suggested it only by its catalog name; a card whose agent gave that action its own label and text keeps offering it, marked custom. When you change a field's type, a value an agent already reported is hidden if it does not fit the new type, until the agent reports it again; a value that still fits, such as text in a field you changed to long text, stays.

Agents can also report fields and actions that are not in the catalog. You see them marked **custom** in the overlay.

## In the TUI

The TUI does not show cards, and it does not draw the tree. Its dashboard keeps showing each agent's status as before. From any shell on the machine, `dot-agent-deck card list` prints every card as JSON, with the same status, needs-you reasons and actions the app shows, and `dot-agent-deck card act` presses one action. Each card in the list also carries the tree the app draws: `parent` names the key of the card that started it (a card with no `parent` is in the top row), `kind` says whether it is a dispatcher, orchestrator, role or agent, `rollup` counts the agents under it by status with the most urgent one and how many need you, and `checked_refs` lists the pull requests and issues the agent reported, each with what the agent wrote and what the deck found. The cards are listed in the tree's order, each one before the agents it started.

```sh
dot-agent-deck card act --entry <key> --action <id> --token <token> --instance <instance_id>
```

Take every value from `card list`: the card's `key`, the action's `id` and `token`, and the list's `instance_id`. The token says which version of the action you read, so if the agent changed the action since, nothing is sent and the command says so; list the cards again. Add `--confirm` for an action that asks for confirmation and `--text` for **Reply**. Run `card act` from your own shell, not from an agent's pane: the deck refuses it there, so that one agent does not press another agent's buttons by mistake.
