# Agent Cards

Agent cards are a view of the desktop app's Dashboard. Each agent the daemon runs gets a card that says what it is doing, whether it needs you, and which pull requests and issues it reported. Click a card to open it and act on it, such as answering its question or telling it to go ahead. The cards are laid out as a tree: you see the agents you started, and you open one to see the agents it started. You read many agents at a glance and act on the one that needs you without opening its terminal.

## What a card shows

A card holds two kinds of information, and the app labels each so you can tell them apart:

- **What the deck checked.** The agent's status (Working, Needs input, Idle, Stopped and the rest), which agent started it, the pull request of its branch, and the pull requests and issues the agent reported, looked up on GitHub by the deck. The deck works these out itself. The overlay marks them **deck checked**.
- **What the agent says.** A short summary, its own status (working, waiting, done or failed), named fields such as its progress or the pull request it opened, questions for you, and actions it suggests. These are the agent's own words and the deck does not check them. They are marked **agent says**.

An agent that has reported nothing yet still has a card, with its deck-checked status and the deck's own actions, and the words "No report from this agent yet." in its overlay. An agent that clears its card (`dot-agent-deck card publish --clear`) goes back to that.

A card is marked **Needs you** when its agent stopped, is asking a question in its terminal or on its card, says it is waiting for you, done or failed, or is blocked. Hover the mark to see which.

The mark for a report that the agent is waiting, done or failed goes away once you have dealt with it: when the agent starts working again after that report (you replied, or it moved on by itself), or when you open the agent after it (its pane in the desktop app, from the Dashboard or the deck, or in the TUI). The card keeps the report; only the mark goes. The agent's next waiting, done or failed report marks it again. The other reasons stay for as long as they are true: a stopped agent, an unanswered question and a block keep the mark however often you open the agent. With a daemon from an earlier release, the mark stays until the agent publishes a new report.

## Switch between the table and the cards

The Dashboard's top bar has a **Table | Cards** switch. **Table** is the Dashboard you know. **Cards** shows the cards. The app remembers your choice in this window's storage, so the next time you open the Dashboard it shows the same view.

## The tree of cards

Under each daemon's header, the Cards view shows a row of small cards of the same size, one for each agent nothing else started. Each card's top line has the agent's name, **Needs you** when it needs you, an arrow when it has agents under it, and the **×** Close button at its right end. Under that line are what the agent is and its status, then its checked pull requests and issues, then an issue's labels. A closed card with agents under it also has a coloured bar and a line such as "4 under it · 1 needs you". When you open a card with its arrow, the cards of the agents under it appear in a row below it, joined to it by a line, and the cards in the top row stay where they were.

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

- **The agent's name.** Click it to open the agent's terminal, as clicking its row in the Table view does.
- **What it is** — dispatcher, orchestrator, role or agent — then its status in the same words as the table and the TUI.
- **Needs you**, when it does.
- **The pull requests and issues the agent reported, as the deck checked them** (see [Pull requests and issues on a card](#pull-requests-and-issues-on-a-card)). When there are more than fit, **+N** at the end of the line counts the rest; press it to open the card's overlay at its list of every one.
- **An issue's labels**, under the pull requests and issues, as small chips with a dot in each label's colour on GitHub. When there are more than fit, **+N** counts the rest, and hovering it names them; the overlay lists them all. Labels are for reading: clicking one does what clicking the card does.
- **The bar and count** of the agents under it, on a card that has any.
- **Close**, the **×** button, which closes the agent exactly as **Close** in the overlay does: it shows what is still pending and asks before it closes anything. It is disabled when the overlay's **Close** would be, and hovering it says why. It is the only action on the card.

Click anywhere else on a card to open its overlay, which has everything the agent reported and every action you can take. From the keyboard, press `Tab` to reach a card's controls: its name, its status line, which opens the overlay, its pull request and issue badges, **+N**, the tree's arrow and **Close**. Each control does only its own thing: clicking a badge opens its page, not the overlay.

The agent's summary, progress, next step, agent type (such as Claude Code or Codex) and other fields are not on the card: they are in its overlay.

Each daemon keeps its own section with its header, so its **Stop daemon**, **Upgrade**, host figures and **New agent** buttons are there in both views, and its cards are below the header.

In the Table view, a row whose card needs you shows an amber **Needs you** badge after the agent's name and its pull request badge; hover it to see why. The badge is not a button: clicking it opens the agent like the rest of the row. The row's **⋯** menu has **Show card**, which opens that agent's card, and the action the agent most needs from you, such as the answer to its question. The menu shows only the action's name, so choosing it sends nothing yet: it opens the card at a step that shows exactly what the action sends, and you send it from there. Clicking the row itself still opens the agent's terminal, as before, and so does `Enter` or `Space` on a row focused with `Tab`.

## Pull requests and issues on a card

When an agent reports a pull request or an issue on its card, the deck looks it up on GitHub itself and shows what GitHub says, not what the agent wrote. A pull request shows the same badge as the Dashboard's [pull request badge](dashboard.md#pull-requests): its state, its CI checks and its review. An issue shows its number and whether it is open or closed, and its labels, in their GitHub colours, on the line under it. A label whose colour the app cannot use is shown in a neutral grey. Hover a badge to read it in words, with when the deck checked it and what the agent wrote. The deck checks open pull requests and issues again from time to time, and eventually stops checking one that is merged or closed.

Click a badge to open GitHub's page for it inside the app, as the Dashboard's pull request badge does (see [Read a pull request in the app](dashboard.md#read-a-pull-request-in-the-app)). The card overlay closes so that the page is in front. This works on a stopped agent's card as well, for as long as the daemon lists that card. If the page cannot be opened, the app says why, on the overlay or above the cards: for example, "That card is no longer on its daemon, so its page was not opened."

What the deck reads, and how:

- It reads the `pull-request` and `issue` fields, and any other field of type `pull_request` or `issue`, whether the catalog defines it or the agent reports it as a custom field. A number such as `#123` in the agent's summary or other text is not read: ask the agent to put it in one of those fields.
- It reads `#123`, `owner/repo#123` and a github.com address such as `https://github.com/owner/repo/pull/123` or `https://github.com/owner/repo/issues/123`. A plain `#123` means that number in the repository the agent is working in, the GitHub `origin` of its folder. An address says for itself whether it is a pull request or an issue, so a pull request address in the `issue` field is shown as the pull request it is.
- It looks them up with the `gh` command on the machine the daemon runs on, with that machine's GitHub login. That is the machine that needs `gh` installed and logged in, not the one you run the app on.

Until the first lookup finishes, the badge reads **checking…**. A badge marked **stale** shows what the deck found last time, because its latest check failed; hover it to see when that was. When the deck could not check one, the card shows why in a few words, with the full sentence when you hover it and in the overlay:

| The card shows | What it means | What to do |
| --- | --- | --- |
| no GitHub access | `gh` is missing or not logged in on the deck's host. | Install `gh` on the machine the daemon runs on and run `gh auth login` there. |
| not found or private | GitHub has nothing by that number that the deck's login can see. It may not exist, or it may be private to a login the deck's host does not use. | Check the number and the repository. For a private repository, log `gh` in on the deck's host with an account that can see it, or give its token access to that repository. |
| GitHub rate limit | The deck's host has used up its GitHub requests for now. | Wait: the deck tries again later by itself. |
| check failed | The deck's host could not check it for another reason, such as a repository that was renamed or moved. | Read the deck's log ([Troubleshooting → Enabling Debug Logs](../troubleshooting.md#enabling-debug-logs)). For a renamed repository, ask the agent to report the repository's current name. |
| not a GitHub reference | The agent wrote something the deck cannot read as a GitHub pull request or issue, or a `#123` in a folder with no GitHub `origin`. | For a `#123` in a folder with no GitHub `origin`, ask the agent to report `owner/repo#123` or the github.com address. Otherwise, ask it to report `#123`, `owner/repo#123` or the github.com address. |

## The card overlay

Click a card anywhere but its name and buttons, or **Show card** in a row's menu, to open the card overlay. It starts with what you need to answer the agent: its summary, its questions with their answers as buttons, and a box to reply in your own words. Below those are the pull requests and issues it reported, every field, and every action with the exact text it sends to the agent, why it is disabled when it is, and what happened to the last time you pressed it. Its header has the deck's status and the agent's type. **Sent from this card** lists the last actions you sent the agent, each with the exact text and whether it was delivered, and keeps them after the agent's card stops offering those actions.

**Pull requests and issues**, marked **deck checked**, lists every pull request and issue the agent reported, in the order of its fields. Each one has its badge, which opens it as on the card, what the deck found (for example `Issue #3: closed`, or a pull request's state, review and CI) with its title on GitHub, every label of an issue, when the deck checked it, that the check may be out of date, that it is still to be checked, or why it cannot be checked, and what the agent wrote, marked **agent says**.

The text an action sends is shown exactly as it is sent, set off as a quotation under the action's button. Characters you could not otherwise see, such as a tab, a character that changes the direction of the text around it, or one that joins or alters the characters next to it, are shown as a boxed code between `⟦` and `⟧`, like `⟦tab⟧` or `⟦U+202E⟧`, with a note under the text saying what the codes mean; hover a code to see what it stands for. Nothing is left out. A `⟦` the agent typed is itself shown as the code `⟦U+27E6⟧`, so ordinary text can never pass for one of these codes, and text such as `\t` that the agent typed is shown as the plain characters it is.

- **[** and **]**, or the arrow buttons, move to the previous and next card without closing the overlay, in the tree's order: each agent comes after the one that started it, and cards inside a closed tree are included. **n**, or **Next needing you**, jumps to the next card that needs you. **Escape** or the **×** closes it.
- The overlay updates while it is open: when the agent publishes a new report or a message is delivered, you see it straight away. If the agent is closed while you look at its card, the overlay says "This agent was closed." and you can move on to the next card.
- An action whose text the agent wrote, such as one it suggested that is not in your catalog or an answer to one of its questions, does not send when you press its button: the overlay opens a step at its top that shows the whole text, and **Send** there sends it. Actions from your catalog send when you press them.
- An action marked **Asks you to confirm first**, and both ways of closing the agent, show you exactly what will be sent or done and ask before doing it. Closing tells you what is still pending, such as an unanswered question or an open pull request.
- If the agent changes an action while you are looking at it, the confirm step shows the new text, says it changed, and sends nothing until you confirm you have read it. A button pressed after the agent changed its action is refused, and nothing is sent; look at the card again and press it once more if you still want it.
- The reply box is there whenever the agent can take a reply: type your own message and press **Send reply**. It is sent to the agent exactly as you typed it. **Reply** among the actions puts the cursor in that box.
- **Open** closes the card and opens the agent's terminal pane. **Close** closes the agent, and **Close, keep worktree** closes a dispatched agent but keeps its copy of the repository, when it has one.
- Actions that send the agent a message are disabled while the agent is not running, and the overlay says so.
- What happened to a message you sent reads **Queued for the agent**, **Delivered to the agent**, **Not delivered** with the reason, or **Delivery uncertain** when the deck cannot tell whether the agent got it, or has run out of attempts to send it, or has seen the agent hold it for a long time without starting on it. An uncertain message is also in the agent's inbox, where you can retry or cancel it.

An action is always sent to the agent whose card it is on, and only to that agent, with the text the card showed you. The agent is told that you pressed it on its card; when the text is, or quotes, what the agent itself wrote on the card (an action or answer it worded, or an answer that repeats its question), the agent is told that too, and gets that text marked as untrusted, so it does not take its own words for yours or for a message from the deck. If the agent was replaced (its pane restarted) after you opened its card, or the deck's daemon was restarted, the action is refused rather than sent to whichever agent has its place now.

## Send one action to many agents

You can send one action to many agents at once, for example **Go ahead** to every agent in an orchestration, or **Close** to a handful of finished agents. Every such batch shows you who gets what before anything is sent.

There are two ways to start one:

- **From a card with agents under it.** Open the card's overlay. Below its actions, a section named after the card and the agents under it has a button for each action that card or any agent under it offers. Pressing one starts a batch for that card and every agent under it, however deep, whether or not its tree is open.
- **From a selection.** Press **Select** in the Cards toolbar. Each card gets a checkbox, and the toolbar shows how many cards are selected and a button for each action the selected cards offer. Selecting a closed card selects every agent under it as well, and clearing it clears them; with its tree open, its checkbox selects that card alone, so you can pick agents inside the tree one by one. Press **Select** again to stop selecting and clear the selection.

Either way, a confirmation opens before anything is sent, even for an action that would otherwise send straight away. It lists:

- each agent that gets the action, with the exact text it will receive, shown as a quotation in the same way as in the card overlay (each agent may get different text, for example an action an agent worded itself);
- each agent that is skipped, and why: its agent is not running, its card does not offer that action, the action is disabled on it, or the agent is no longer there.

When the batch reaches agents on more than one daemon, each agent in the list says which daemon it is on, so two agents with the same name are told apart.

Press **Send** to send it to the agents listed as getting it, or **Cancel** (or `Escape`) to send nothing. While the batch is being sent, the dialog stays open: **Cancel**, `Escape` and a spoken "cancel" say it is still sending rather than close it, and another batch cannot be started until this one's results are in. After **Send**, the dialog shows what happened for each agent: **Delivered**, **Closed**, or **Failed** with the reason. One agent failing does not stop the others. An agent whose action changed after the confirmation opened, or whose daemon restarted, is not sent anything, not even the new text; it is marked **Failed**, and the others still get theirs. Each delivery is recorded on that agent's own card, under **Sent from this card**, as if you had pressed the action on that card.

Some actions are not offered for a batch: **Open**, a reply in your own words, and the answers to one agent's own question are pressed on each card. A batch reaches the agents of one daemon or of several, when your selection spans daemons. A daemon from an earlier release does not offer batches: its cards have no checkbox and its overlays no batch section, and you act on its agents one card at a time.

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
| "open the coder" | Opens that agent's terminal, as clicking its name on its card does. |
| "press go ahead on the coder's card", "answer main to the coder", "press close on the coder's card" | Presses that action or answer, named by the label on its button. Close opens the card at its confirm step, as the card's **×** does. |
| "go ahead on everything under the dispatcher", "close every agent under the planner" | Starts a batch for that agent and every agent under it, and opens its confirmation. Nothing is sent until you press **Send**. |
| "select the coder's card", "add the tester to the selection", "unselect the tester's card" | Selects that agent's card for a batch, or takes it out of the selection, as its checkbox does: a card whose tree is closed takes every agent under it along. Selecting shows the cards and turns selection on. Say "card" or "selection": "select the coder" on its own opens the coder. |
| "go ahead on the selected agents", "close the selected ones" | Starts a batch for every selected agent and opens its confirmation. Nothing is sent until you press **Send**. |
| "reply to the coder use the main branch" | Opens that agent's card with your words in its **Reply** box, for you to read, change and send with **Send reply**. |

When the agent reported more than one pull request, or more than one issue, voice opens the first one the deck could check; click a badge to open another. If the dashboard's filter hides the card whose tree you asked to open, the app opens it anyway and tells you to clear the filter to see it.

With **All daemons** selected, you can name an agent on any daemon the Dashboard shows. When the name fits agents on more than one daemon, the app asks which; add the daemon's name, as in "show the coder's card on build box", to pick it straight away (see [Naming an agent](voice.md#naming-an-agent)).

Voice sends an action straight away only when its text comes from your catalog. Anything with text the agent wrote opens the card at a step showing exactly what would be sent, and the voice row says so; press **Send** to send it. That includes an action the agent suggested that is not in your catalog and every answer to one of its questions: an answer the agent gave no text of its own sends `Answer to your question "<question>": <answer>`, which repeats the agent's question and answer words. An action that asks you to confirm, and closing an agent, are never done by voice alone either: the card opens at its confirm step and you press the button yourself. A batch is never sent by voice alone: voice opens its confirmation, and you press **Send**. By voice, you select agents by name, or name the agent whose tree the batch reaches. With **All daemons** selected, a selection can take agents from every daemon the dashboard shows. A spoken reply is not sent by voice alone, as dictating to an agent's terminal types your words without pressing Enter: you see what was heard before it goes.

## Which agents publish a card

An agent publishes its card by running `dot-agent-deck card publish` in its pane, so any supported agent that can run a command (Claude Code, Codex, OpenCode, Pi and Devin) can publish one. An agent kept by its own sandbox from running `dot-agent-deck` cannot, and its card shows only what the deck checked.

When the deck dispatches work, the task it gives the agent explains how to keep its card current and lists the fields and actions it can use. An agent you start yourself is not told: ask it to run `dot-agent-deck card publish --help` and `dot-agent-deck card catalog`. Publishing a card notifies nobody; it is not the agent's report that its task is done.

## Choose the fields and actions agents use

The fields agents fill in and the actions they can offer you come from a catalog. The deck has defaults: the fields `progress`, `next-step`, `details`, `pull-request`, `issue` and `links`, and the actions **Go ahead** and **Merge it**. `dot-agent-deck card catalog` prints the catalog the daemon uses. Earlier releases also had an **Explain** action on every card; to keep it, add it to your catalog as the example below shows. A `remove = ["explain"]` you wrote to hide it still works and needs no change; the deck ignores it without a warning.

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

[[catalog.actions]]                # the Explain action earlier releases offered on every card
name = "explain"
description = "Ask the agent to explain what it did and what it needs."
prompt = "Explain in a few sentences what you did and what you need from me, then update your card."
always = true
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

The TUI does not show cards, and it does not draw the tree. Its dashboard keeps showing each agent's status as before. Opening an agent's pane in the TUI (`Enter` on its card, or its number) counts as opening it, so it clears the same **Needs you** in `card list` and in the desktop app. From any shell on the machine, `dot-agent-deck card list` prints every card as JSON, with the same status, needs-you reasons and actions the app shows, and `dot-agent-deck card act` presses one action. Sending one action to many agents at once is done in the desktop app; from a shell, `card act` acts on one card at a time. Each card in the list also carries the tree the app draws: `parent` names the key of the card that started it (a card with no `parent` is in the top row), `kind` says whether it is a dispatcher, orchestrator, role or agent, `rollup` counts the agents under it by status with the most urgent one and how many need you, and `checked_refs` lists the pull requests and issues the agent reported, each with what the agent wrote and what the deck found, including an issue's `labels`. The cards are listed in the tree's order, each one before the agents it started.

```sh
dot-agent-deck card act --entry <key> --action <id> --token <token> --instance <instance_id>
```

Take every value from `card list`: the card's `key`, the action's `id` and `token`, and the list's `instance_id`. The token says which version of the action you read, so if the agent changed the action since, nothing is sent and the command says so; list the cards again. Add `--confirm` for an action that asks for confirmation and `--text` for **Reply**. Run `card act` from your own shell, not from an agent's pane: the deck refuses it there, so that one agent does not press another agent's buttons by mistake.
