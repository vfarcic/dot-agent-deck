# Agent Cards

Agent cards are a view of the desktop app's Dashboard. Each agent the daemon runs gets a card that says what it is doing, whether it needs you, and which pull requests and issues it reported. Click a card to open its agent's pane on its **Terminal** tab; the agent's report, its questions and its actions are on the pane's **Card** tab, where you act on them, such as answering its question or telling it to go ahead. The cards are laid out as a tree: you see the agents you started, and you open one to see the agents it started. You read many agents at a glance and act on the one that needs you without opening its terminal.

## What a card shows

A card holds two kinds of information, and the app labels each so you can tell them apart:

- **What the deck checked.** The agent's status (Working, Needs input, Idle, Stopped and the rest), which agent started it, the pull request of its branch, and the pull requests and issues the agent reported, looked up on GitHub by the deck. The deck works these out itself. The Card tab marks them **deck checked**.
- **What the agent says.** A short summary, its own status (working, waiting, done or failed), named fields such as its progress or the pull request it opened, questions for you, and actions it suggests. These are the agent's own words and the deck does not check them. They are marked **agent says**.

An agent that has reported nothing yet still has a card, with its deck-checked status and the deck's own actions, and the words "No report from this agent yet." in its Card tab. An agent that clears its card (`dot-agent-deck card publish --clear`) goes back to that.

A card is marked **Needs you** when its agent stopped, is asking a question in its terminal or on its card, says it is waiting for you, done or failed, or is blocked. Hover the mark to see which.

The mark for a report that the agent is waiting, done or failed goes away once you have dealt with it: when the agent starts working again after that report (you replied, or it moved on by itself), or when you open the agent after it (its pane in the desktop app, from the Dashboard or the deck, or in the TUI). A turn Claude Code starts by itself only to tell the agent that one of its background commands or subagents finished does not count as starting work again. The card keeps the report; only the mark goes. The agent's next waiting, done or failed report marks it again. The other reasons stay for as long as they are true: a stopped agent, an unanswered question and a block keep the mark however often you open the agent. With a daemon from an earlier release, the mark stays until the agent publishes a new report.

## Switch between the table and the cards

The Dashboard's top bar has a **Table | Cards** switch. **Table** is the Dashboard you know. **Cards** shows the cards. The app remembers your choice in this window's storage, so the next time you open the Dashboard it shows the same view.

## The tree of cards

Under each daemon's header, the Cards view shows a row of small cards of the same size, one for each agent nothing else started. Each card's top line has a checkbox for selecting it (on a daemon that can send one action to many agents), the agent's name, a bell when it needs you, an arrow when it has agents under it, and the **×** Close button at its right end. Under that line are what the agent is and its status, then its checked pull requests and issues, then an issue's labels. A closed card with agents under it also has a coloured bar and a line such as "4 under it · 1 needs you". When you open a card with its arrow, the cards of the agents under it appear below it in a box, joined to it by a line, and the cards in the top row stay where they were.

The Cards view starts with the agents nothing else started: a dispatcher, an orchestration you started yourself (its orchestrator's card), and an agent you started on its own. The agents they started are under them:

- A dispatcher's card has the agents it dispatched under it. When it dispatched an orchestration, that orchestration's orchestrator is under the dispatcher, and its roles are under the orchestrator.
- An orchestrator's card has the orchestration's roles under it.
- A card whose starter was closed moves up to the top row.

Press the arrow on a card to open it and see the agents under it, and press it again to close it. A card under another one opens the same way, as deep as the agents go. Only a card with agents under it has the arrow.

The cards in the top row stay where they are. They fill a row and wrap to the next, in the order their agents started, and they do not move when a status changes, when an agent comes to need you, or when you open one. An open card's tree is drawn below its row in a box of its own, with the card's name at its top left, and joined to the card by a line; when you open more than one card in a row, each tree gets its own box below that row, in the cards' order, and the rows below move down to make room. Every box has the same plain background, so the colour on a card always means its status.

Inside a box, the cards under an open card sit side by side when they fit. When there are more than fit the window's width, they wrap into as many rows as needed, at every level: a line drops from the card above, and each row hangs from it by a line of its own, so you can still tell which card started which. A big tree grows downwards, and the Cards view does not scroll sideways in a window wide enough for one card.

A closed card with agents under it tells you about them:

- **Its colour** is the most urgent status among the card and every agent under it, so a dispatcher whose unit needs input is coloured as needing input. The order, most urgent first: Needs input, Blocked, Error, Waiting, Working, Thinking, Compacting, Idle, Stopped.
- **A bar** shows how many agents under it are in each status, in the same colours.
- **A line** such as "4 under it · 1 needs you" counts every agent under it, at every depth, and how many of them need you.

These update while you watch. An open card keeps its bar and line and takes its own status's colour, since the agents under it are on screen.

The app remembers which cards you opened, for each daemon, in this window's storage, so the next time you open the Dashboard the same trees are open, as long as that daemon has not been restarted.

### Needs you only

**Needs you only** is in the **⋯** menu at the right end of the Dashboard's top bar, beside **Refresh**. A small number on that **⋯** says how many agents need you. Choose **Needs you only** and the Cards view opens the way to every card that needs you and fades every card that neither needs you nor leads to one. A tick beside it in the menu, and a **Needs you only ×** chip beside **Filter**, show that it is on. Press the chip, or choose the item again, to show every card as usual.

While it stays on, a card that comes to need you is opened up to as well. A tree you close stays closed until an agent under it newly needs you, and then it opens again.

It works in the Table view too, where it lists only the agents that need you and the agents above them in their tree, and hides the other rows instead of fading them. The setting is the same in both views: switching between them keeps it. When nothing needs you, the Dashboard says so.

### The dashboard's filter

The Dashboard's filter applies to the cards as it does to the table rows: with a status, kind, agent type, daemon or search text chosen, only the cards of the agents it shows are on screen. A card whose starter the filter hides is drawn in the top row until you clear the filter.

### A daemon from an older release

A daemon from a release before the tree was added sends no tree: the app shows its cards side by side, without the bar, the counts or checked pull requests and issues, and says "This daemon is older than the app, so its cards are shown side by side rather than as a tree." Update dot-agent-deck on the machine that daemon runs on to get the tree. A daemon from a release before cards were added has no cards and keeps its usual table.

## What is on a card

Each card has fixed-size lines, so cards line up and stay the same size whatever their agents report. A line too long for the card is cut short, and hovering it shows all of it.

- **The agent's name.** Click it to open the agent's pane, as clicking anywhere else on the card does.
- **What it is** — dispatcher, orchestrator, role or agent — then its status in the same words as the table and the TUI.
- **A bell** when it needs you. Hover it to see why.
- **A checkbox**, on every card whose daemon can send one action to many agents. Tick it to select the card (see [Send one action to many agents](#send-one-action-to-many-agents)). Ticking it does not open the card.
- **The pull requests and issues the agent reported, as the deck checked them** (see [Pull requests and issues on a card](#pull-requests-and-issues-on-a-card)). When there are more than fit, **+N** at the end of the line counts the rest; press it to open the agent's pane on its Card tab, at its list of every one.
- **An issue's labels**, under the pull requests and issues, as small chips with a dot in each label's colour on GitHub. When there are more than fit, **+N** counts the rest, and hovering it names them; the Card tab lists them all. Labels are for reading: clicking one does what clicking the card does.
- **The bar and count** of the agents under it, on a card that has any.
- **Close**, the **×** button, which closes the agent: it shows what is still pending and asks before it closes anything. It is disabled when the agent cannot be closed from its card, and hovering it says why.
- **An icon for each action the agent offers right now**, at the bottom right of the card, beside the count of the agents under it when it has any: **Go ahead**, **Merge it** and any other action that sends the agent a message. The icons sit together in a tinted box with a border, so you can find a card's actions at a glance; the same box holds the actions in the selection bar and in the agent's Card tab. The card's own **Close** is not in it; in the selection bar, closing the agents is one of the icons in the box. Each draws the icon its catalog entry names (see [Choose the fields and actions agents use](#choose-the-fields-and-actions-agents-use)); an action with no icon, or one the app does not have, gets one plain message icon. An action whose placement is **Only in the Card tab** has no icon on the card: its icon is in the agent's Card tab. An icon under the pointer is drawn filled and outlined, and one reached with `Tab` has a ring around it. Hover an icon, or reach it with `Tab`, to see the action's label and the exact text it sends, whether it asks before sending, and why it is disabled when it is; a disabled icon is drawn faded and still reachable with `Tab`, so you can read why. That text appears just below the icon, or just above it when there is no room below. `Esc` hides it. Pressing an icon, on a card or in the Card tab: an action from your catalog is sent straight away, while one that asks first, and one whose text the agent wrote, open a small review that shows the exact text, so nothing is sent until you press **Send** there (see [Review before sending](#review-before-sending)). A text the hover cannot show whole, because it is too long or has more lines than the window has room for, is never sent straight from the icon: the hover ends by saying so, and pressing the icon opens the review, which shows all of it, even if you make the window larger before you press it. The hover follows its text and the window while it is shown: if the action's text changes or you resize the window, it is drawn again to fit. This is a safeguard for pressing the icon: saying the action's name by voice sends an action from your catalog that does not ask first straight away, whatever the length of its text. A card shows up to three icons; when its agent offers more, **+N** counts the rest and opens the agent's pane on its Card tab, at its actions, which has them all. A reply in your own words, **Open** and the answers to the agent's own questions are not icons: you type a reply into the agent's terminal, the card itself opens the agent, and the answers are buttons in the Card tab.

Click anywhere else on a card, or its name, to open the agent's pane. It opens on the agent's terminal; its **Card** tab has everything the agent reported and every action it offers (see [The Card tab](#the-card-tab)). From the keyboard, press `Tab` to reach a card's controls: its checkbox, its name, the tree's arrow, **Close**, its status line, which opens the agent too, its pull request and issue badges, its action icons and **+N**. Each other control does only its own thing: ticking the checkbox, pressing **Close** or an action icon, or clicking a badge opens no pane.

The agent's summary, progress, next step, agent type (such as Claude Code or Codex) and other fields are not on the card: they are in its Card tab.

Each daemon keeps its own section with its header, so its **Stop daemon**, **Upgrade**, host figures and **New agent** buttons are there in both views, and its cards are below the header.

In the Table view, a row whose card needs you shows an amber **Needs you** badge after the agent's name and its pull request badge; hover it to see why. The badge is not a button: clicking it opens the agent like the rest of the row. The row's **⋯** menu has **Show card**, which opens that agent's pane on its Terminal tab, and the action the agent most needs from you, such as the answer to its question. The menu shows only the action's name, so choosing it sends nothing yet: it opens the agent's pane, on its Terminal tab, under a review that shows exactly what the action sends, and you send it from there. Clicking the row itself opens the agent's pane on its Terminal tab too, and so does `Enter` or `Space` on a row focused with `Tab`.

## Pull requests and issues on a card

When an agent reports a pull request or an issue on its card, the deck looks it up on GitHub itself and shows what GitHub says, not what the agent wrote. A pull request shows the same badge as the Dashboard's [pull request badge](dashboard.md#pull-requests): its state, its CI checks and its review. An issue shows its number and whether it is open or closed, and its labels, in their GitHub colours, on the line under it. A label whose colour the app cannot use is shown in a neutral grey. Hover a badge to read it in words, with when the deck checked it and what the agent wrote. The deck checks open pull requests and issues again from time to time, and eventually stops checking one that is merged or closed.

Click a badge to open GitHub's page for it inside the app, as the Dashboard's pull request badge does (see [Read a pull request in the app](dashboard.md#read-a-pull-request-in-the-app)). The page opens in front of the Dashboard, or of the agent's pane when you click it in the Card tab; closing the page takes you back. This works on a stopped agent's card as well, for as long as the daemon lists that card. If the page cannot be opened, the app says why, in the Card tab or above the cards: for example, "That card is no longer on its daemon, so its page was not opened."

What the deck reads, and how:

- It reads the `pull-request` and `issue` fields, and any other field of type `pull_request` or `issue`, whether the catalog defines it or the agent reports it as a custom field. A number such as `#123` in the agent's summary or other text is not read: ask the agent to put it in one of those fields.
- It reads `#123`, `owner/repo#123` and a github.com address such as `https://github.com/owner/repo/pull/123` or `https://github.com/owner/repo/issues/123`. A plain `#123` means that number in the repository the agent is working in, the GitHub `origin` of its folder. An address says for itself whether it is a pull request or an issue, so a pull request address in the `issue` field is shown as the pull request it is.
- It looks them up with the `gh` command on the machine the daemon runs on, with that machine's GitHub login. That is the machine that needs `gh` installed and logged in, not the one you run the app on.

Until the first lookup finishes, the badge reads **checking…**. A badge marked **stale** shows what the deck found last time, because its latest check failed; hover it to see when that was. When the deck could not check one, the card shows why in a few words, with the full sentence when you hover it and in the Card tab:

| The card shows | What it means | What to do |
| --- | --- | --- |
| no GitHub access | `gh` is missing or not logged in on the deck's host. | Install `gh` on the machine the daemon runs on and run `gh auth login` there. |
| not found or private | GitHub has nothing by that number that the deck's login can see. It may not exist, or it may be private to a login the deck's host does not use. | Check the number and the repository. For a private repository, log `gh` in on the deck's host with an account that can see it, or give its token access to that repository. |
| GitHub rate limit | The deck's host has used up its GitHub requests for now. | Wait: the deck tries again later by itself. |
| check failed | The deck's host could not check it for another reason, such as a repository that was renamed or moved. | Read the deck's log ([Troubleshooting → Enabling Debug Logs](../troubleshooting.md#enabling-debug-logs)). For a renamed repository, ask the agent to report the repository's current name. |
| not a GitHub reference | The agent wrote something the deck cannot read as a GitHub pull request or issue, or a `#123` in a folder with no GitHub `origin`. | For a `#123` in a folder with no GitHub `origin`, ask the agent to report `owner/repo#123` or the github.com address. Otherwise, ask it to report `#123`, `owner/repo#123` or the github.com address. |

## The Card tab

An agent's pane has a **Card** tab beside its **Terminal** tab, showing the agent's card in the terminal's place. Click **Card** to see it, and **Terminal** to go back; the agent's terminal keeps running underneath and keeps its size, so switching never disturbs the agent.

The pane opens on **Terminal** however you open it: clicking a card or its name, a table row or **Show card** in its **⋯** menu, saying "open the coder", or moving with **Previous agent**, **Next agent** or **Next needing you**. The app does not remember that you last looked at the card: the next time you open any agent, the same one included, its pane starts on **Terminal** again. Two ways in open on **Card**, because they ask for the card itself:

- A card's **+N**: the one after its pull requests and issues opens the Card tab at its list of every pull request and issue, and the one after its action icons opens it at its actions, in each case with the keyboard on that list, so `Tab` continues into it.
- Saying "show the coder's card", from anywhere on the Dashboard: the Dashboard itself, or any agent's pane open over it, another agent's included.

While the agent needs you, the **Card** tab shows a bell, and a screen reader reads it as "Card, needs you".

The Card tab has two parts, each labelled:

- **Status**, marked **deck checked**: the deck's status and the agent's type, why an orchestration role exited and its last output, the pull request of its branch, the **Pull requests and issues** it reported, and **Sent from this card**, the last actions you sent the agent, each with the exact text and whether it was delivered, kept after the card stops offering them.
- **Report**, marked **agent says**: its summary and its own status, or "No report from this agent yet.", why it needs you (the words the card's bell shows when you hover it), its fields, and its questions, each with its answers as buttons. A field is shown as text: a web address the agent put in one is not a link, so select and copy it to open it. The pull requests and issues the deck checked open from their badges.

Under them, **Actions** holds an icon for every action the agent offers, the same icons as on the card, in the same box, with the same text when you hover one, including the ones placed **Only in the Card tab**. Beside the **Actions** heading, **Edit** opens the [Card actions editor](#the-card-actions-editor) of the agent's daemon, as **Card actions** in that daemon's **⋯** menu does. They behave as on the card: an action from your catalog is sent when you press it, and one that asks first or whose text the agent wrote opens the review. Under the icons, the Card tab says what became of each one you pressed. The deck's own actions are not repeated: the pane itself is **Open**, you reply by typing into the agent's terminal, and the pane's **Close agent** closes it.

**Pull requests and issues** lists every pull request and issue the agent reported, in the order of its fields. Each one has its badge, which opens it as on the card, what the deck found (for example `Issue #3: closed`, or a pull request's state, review and CI) with its title on GitHub, every label of an issue, when the deck checked it, that the check may be out of date, that it is still to be checked, or why it cannot be checked, and what the agent wrote, marked **agent says**.

The Card tab updates while you look at it: when the agent publishes a new report or a message is delivered, you see it straight away, and the pane never switches tabs by itself, even when the agent comes to need you. If the agent's card goes, or its daemon stops answering, the Card tab stays and says the card is not available right now, with none of its actions; it shows the card again when the daemon answers.

A card the deck keeps after its agent has gone, such as an orchestration role that exited, opens a pane of its own, on its **Card** tab, since there is no terminal to show. Its header names the agent with the status it last had, and offers the moves between agents and **Back to dashboard** but no **Close agent**. Its **Terminal** tab says, in one sentence, that the agent has ended and its terminal is gone. The Card tab shows why the role exited, its last output, its pull requests and issues and its report. It offers none of the card's actions and none of the answers to its questions, and no **Edit** beside **Actions**, and says so, because there is no agent to send them to; its pull requests and issues still open from their badges. An agent that ends while you have its pane open turns into this pane. If its daemon stops answering, the pane stays and says the card is not available right now; when the daemon answers again, the pane shows the card again, or goes back to the dashboard if the daemon no longer keeps the card. It becomes the agent's own pane again only when the daemon lists the agent again.

The pane's header also moves between agents, in the tree's order, each agent coming after the one that started it, cards inside a closed tree included: the arrows **Previous agent** and **Next agent** open the previous and next agent's pane, and **Next needing you** the next agent that needs you, each on its **Terminal** tab. With the keyboard anywhere in the pane but its terminal, `[`, `]` and `n` do the same; in the terminal, they are typed to the agent as usual. `Escape` or **Back to dashboard** leaves the pane.

The Card tab is in the desktop app only. A daemon from a release before cards were added has no cards, and its agents' panes have no **Card** tab.

## Review before sending

Some actions show you exactly what they will do before anything is sent, in a small review over the Dashboard or the agent's pane:

- An action whose text the agent wrote, such as one it suggested that is not in your catalog or an answer to one of its questions, opens the review with the whole text, and **Send** there sends it. Actions from your catalog send when you press them.
- An action marked **Asks you to confirm first**, and closing the agent with the card's **×**, show you exactly what will be sent or done and ask before doing it. Closing tells you what is still pending, such as an unanswered question or an open pull request.
- An icon whose text its hover could not show whole, and the action a table row's menu offers, open the review too.

The text is shown exactly as it is sent, set off as a quotation. Characters you could not otherwise see, such as a tab, a character that changes the direction of the text around it, or one that joins or alters the characters next to it, are shown as a boxed code between `⟦` and `⟧`, like `⟦tab⟧` or `⟦U+202E⟧`, with a note under the text saying what the codes mean; hover a code to see what it stands for. Nothing is left out. A `⟦` the agent typed is itself shown as the code `⟦U+27E6⟧`, so ordinary text can never pass for one of these codes, and text such as `\t` that the agent typed is shown as the plain characters it is.

If the agent changes the action while the review is open, the review shows the new text, says it changed, and sends nothing until you confirm you have read it. An icon pressed after the agent changed its action is refused, and nothing is sent; look at the card again and press it once more if you still want it. After **Send**, the review keeps the text it sent and says what happened, even when the agent closes or stops offering the action meanwhile, and closes with **Close**; **Cancel** or `Escape` closes it without sending. Actions that send the agent a message are disabled while the agent is not running, and say so.

What happened to a message you sent reads **Queued for the agent**, **Delivered to the agent**, **Not delivered** with the reason, or **Delivery uncertain** when the deck cannot tell whether the agent got it, or has run out of attempts to send it, or has seen the agent hold it for a long time without starting on it. An uncertain message is also in the agent's inbox, where you can retry or cancel it.

An action is always sent to the agent whose card it is on, and only to that agent, with the text the card showed you. The agent is told that you pressed it on its card; when the text is, or quotes, what the agent itself wrote on the card (an action or answer it worded, or an answer that repeats its question), the agent is told that too, and gets that text marked as untrusted, so it does not take its own words for yours or for a message from the deck. If the agent was replaced (its pane restarted) after you opened its card, or the deck's daemon was restarted, the action is refused rather than sent to whichever agent has its place now.

## Send one action to many agents

You can send one action to many agents at once, for example **Go ahead** to every agent in an orchestration, or **Close** to a handful of finished agents. Every such batch shows you who gets what before anything is sent.

Start one from a selection. Tick the checkbox on the cards you want. As soon as one card is ticked, a bar appears at the bottom of the Dashboard. It says how many cards are selected, holds an icon button for each action the selected cards offer, in the same box as a card's icons, and has **Clear**, which unticks them all and hides the bar. Set apart after the actions, a sliders button, **Edit card actions**, opens the [Card actions editor](#the-card-actions-editor) of the daemon the selected cards are on; when they are on more than one daemon, it opens a small menu of those daemons, and you choose whose actions to edit. Pressed from the keyboard, **Clear** leaves you where you were: the card you ticked last has focus while it is still shown, and `Tab` goes on from there. The bar stays at the bottom wherever you scroll, and when you scroll to the end the last card is above it, not under it. Selecting a closed card selects every agent under it as well, however deep, and unticking it unticks them, so ticking one closed card is how you send an action to an agent and everything under it; with its tree open, its checkbox selects that card alone, so you can pick agents inside the tree one by one. By voice, you can also name an agent and send an action to it and everything under it.

The batch buttons draw the same icons as the cards, with **Close** as a cross, in the same tinted box. Hover one, or reach it with `Tab`, to see the action's label and the exact text it sends when every agent it can reach sends the same text, or that the text differs per agent when they do not.

Either way, a confirmation opens before anything is sent, even for an action that would otherwise send straight away. It lists:

- each agent that gets the action, with the exact text it will receive, shown as a quotation in the same way as in the review. When several agents get the same text, it is shown once, followed by the agents that get it; an agent that gets different text, for example an action it worded itself, has its own quotation. Each text is marked **agent says** when an agent wrote it, or **catalog** when it comes from your catalog, and every different text is shown however many agents the batch reaches. The heading counts the agents and, when they get different texts, how many texts there are. When more than ten agents get the same text, the first few of them are named and **+N more** under them names the rest when you press it;
- the agents that are skipped, as one line counting them by reason, such as "214 skipped: 180 don't offer Go ahead, 34 aren't running". Press the line, or reach it with `Tab` and press `Enter`, to list them, grouped by reason, each with why: its agent is not running, its card does not offer that action, the action is disabled on it, or the agent is no longer there. Press it again to fold the list.

When the batch reaches agents on more than one daemon, each agent in the list says which daemon it is on, so two agents with the same name are told apart. If one of those daemons cannot be reached, does not answer, or refuses the batch, its agents are listed as skipped with the reason (a daemon that refused gives its own), and the batch still goes to the agents on the other daemons. If none of them can be reached or answers, the confirmation says nothing was sent and why.

When the agents get more than one different text, **Send** waits until each text has been on screen: scroll through the list, or press **Show next text**, which brings the next text you have not seen into view. Until then, the dialog says beside **Send** how many texts there are to see, for example "Scroll to see all 3 texts". A batch whose agents all get the same text can be sent straight away.

Press **Send** to send it to the agents listed as getting it, or **Cancel** (or `Escape`) to send nothing. While the batch is being sent, the dialog stays open: **Cancel** is disabled, `Escape` and a spoken "cancel" say it is still sending rather than close it, and another batch cannot be started until this one's results are in. After **Send**, the dialog shows what happened for each agent: **Queued for the agent** when its text is in the agent's inbox, **Closed**, or **Failed** with the reason, and a line above the list counts each of those apart. A queued message is where a single press leaves it too: the agent has not confirmed it yet, and the agent's card shows, under **Sent from this card**, when it is delivered or if it is not. If a daemon is slow to reach or stops answering, the app stops waiting after a while (longer for a bigger batch, and never more than three minutes from **Send**) and marks that daemon's agents **No answer**: the daemon may still deliver to them, so check those agents' cards before you send again. The same happens when the connection to a daemon breaks after the batch was sent to it, since some agents may already have it. If the app knows the batch was not sent at all, its agents are marked **Failed** instead; when it cannot tell, they are marked **No answer**. A daemon that started the batch and cannot say what became of an agent, because it was interrupted while sending or no longer remembers the results when you send the same batch again, marks that agent **No answer** too, with its reason. Nothing is sent again on its own, and you can close the dialog. One agent failing does not stop the others. An agent whose action changed after the confirmation opened, or whose daemon restarted, is not sent anything, not even the new text; it is marked **Failed**, and the others still get theirs. Each delivery is recorded on that agent's own card, under **Sent from this card**, as if you had pressed the action on that card.

Some actions are not offered for a batch: **Open**, a reply in your own words, and the answers to one agent's own question are pressed on each card. A batch reaches the agents of one daemon or of several, when your selection spans daemons. A daemon from an earlier release does not offer batches: its cards have no checkbox, and you act on its agents one card at a time.

## By voice

With voice control on, you can say:

| Say | What happens |
| --- | --- |
| "show cards", "show the table" | Switches the Dashboard's view. |
| "show the coder's card" | Opens that agent's pane on its **Card** tab, from anywhere on the Dashboard: the Dashboard itself, or any agent's pane open over it, another agent's included. |
| "open the dispatcher's tree", "close the dispatcher's tree" | Opens or closes the tree under that agent's card. Opening it also opens the way down to that card when it is inside a closed tree. |
| "open the pull request on the coder's card", "open the issue on the coder's card" | Opens, inside the app, the pull request or issue that agent reported, as the deck checked it. |
| "next agent", "previous agent" | In an agent's pane, opens the next or previous agent's pane. |
| "next one that needs me" | Opens the pane of the next agent that needs you, from the Dashboard or an agent's pane. |
| "only the ones that need me", "show all cards", "show every agent" | Turns **Needs you only** on or off, in the Table view or the Cards view, keeping the view you are on. While **Needs you only** is on, "show all", "show all cards" and "show every agent" turn it off; while it is off, "show all cards" on the Table view switches to the Cards view. |
| "show the card" | In an agent's pane, switches to its **Card** tab. |
| "close the card", "show the terminal", "back to the terminal" | In an agent's pane, switches from its **Card** tab back to **Terminal**. With no card showing, the app says so and changes nothing. "Back to dashboard" or "close" leaves the pane. |
| "open the coder" | Opens that agent's pane on its **Terminal** tab. |
| "press go ahead on the coder's card", "answer main to the coder", "press close on the coder's card" | Presses that action or answer, named by the label on its button. Close, and an action that asks first or whose text the agent wrote, open the review, as the card's buttons do. |
| "go ahead on everything under the dispatcher", "close every agent under the planner" | Starts a batch for that agent and every agent under it, and opens its confirmation. Nothing is sent until you press **Send**. |
| "select the coder's card", "add the tester to the selection", "unselect the tester's card" | Selects that agent's card for a batch, or takes it out of the selection, as its checkbox does: a card whose tree is closed takes every agent under it along. Selecting shows the cards. Say "card" or "selection": "select the coder" on its own opens the coder. |
| "clear the selection", "deselect all the cards" | Unticks every selected card and hides the selection bar. |
| "go ahead on the selected agents", "close the selected ones" | Starts a batch for every selected agent and opens its confirmation. Nothing is sent until you press **Send**. |
| "open card actions", "open card actions on build box" | Opens the **Card actions** editor of that daemon. With one daemon on the Dashboard you need not name it. With several, and cards selected on only one of them, it opens that daemon's; otherwise the app asks which. In an agent's pane opened from the Dashboard, "open card actions" opens the editor of that agent's daemon, as the Card tab's **Edit** does; if that daemon has no cards, the app says so. The editor's other commands below then work over the pane too. |
| "add a card action" | Opens the editor's form for a new action, empty, for you to fill in and save. |
| "edit the ship it action" | Opens that action's form in the editor, named by its label. |
| "remove the ship it action" | Opens the editor's confirmation for removing that action of yours. Nothing is removed until you press **Remove**. |
| "refresh the card actions" | Reads the open editor's list again, keeping what the form holds. |
| "icon rocket" | In the editor's form, chooses that icon, by the name its grid shows. |
| "ask before sending" / "send without asking" | In the editor's form, ticks or unticks **Ask before sending**. |
| "offer on every card" / "only when agents suggest it" | In the editor's form, ticks or unticks **Offer on every card**. |
| "show it on the card" / "only in the Card tab" | In the editor's form, sets **Placement** to **Show on the card** or **Only in the Card tab**. |

In an agent's pane opened from the Daemons screen, the commands that move between agents, show an agent's card, press a card's action or open the **Card actions** editor do nothing: the app says they work in a pane opened from the Dashboard.

When the agent reported more than one pull request, or more than one issue, voice opens the first one the deck could check; click a badge to open another. If the dashboard's filter hides the card whose tree you asked to open, the app opens it anyway and tells you to clear the filter to see it.

With **All daemons** selected, you can name an agent on any daemon the Dashboard shows. When the name fits agents on more than one daemon, the app asks which; add the daemon's name, as in "show the coder's card on build box", to pick it straight away (see [Naming an agent](voice.md#naming-an-agent)).

Voice sends an action straight away only when its text comes from your catalog. Anything with text the agent wrote opens the review showing exactly what would be sent, and the voice row says so; press **Send** to send it. That includes an action the agent suggested that is not in your catalog and every answer to one of its questions: an answer the agent gave no text of its own sends `Answer to your question "<question>": <answer>`, which repeats the agent's question and answer words. An action that asks you to confirm, and closing an agent, are never done by voice alone either: the review opens and you press the button yourself. A batch is never sent by voice alone: voice opens its confirmation, and you press **Send**. By voice, you select agents by name, or name the agent whose tree the batch reaches. With **All daemons** selected, a selection can take agents from every daemon the dashboard shows. In the **Card actions** editor, voice opens the editor of the daemon you name and its forms, reads the list again and sets the form's icon, checkboxes and placement, but every change to the file is a press of yours: **Save**, **Remove**, **Hide** and **Restore** are never done by voice, so a misheard word never becomes text an agent is sent. To reply in your own words, open the agent ("open the coder") and say "tell it …": your words are typed into its terminal without pressing Enter, so you see what was heard before it goes.

## Which agents publish a card

An agent publishes its card by running `dot-agent-deck card publish` in its pane, so any supported agent that can run a command (Claude Code, Codex, OpenCode, Pi and Devin) can publish one. An agent kept by its own sandbox from running `dot-agent-deck` cannot, and its card shows only what the deck checked.

When the deck dispatches work, the task it gives the agent explains how to keep its card current and lists the fields and actions it can use. An agent you start yourself is not told: ask it to run `dot-agent-deck card publish --help` and `dot-agent-deck card catalog`. Publishing a card notifies nobody; it is not the agent's report that its task is done.

## Choose the fields and actions agents use

The fields agents fill in and the actions they can offer you come from a catalog. The deck has defaults: the fields `progress`, `next-step`, `details`, `pull-request`, `issue` and `links`, and the actions **Go ahead** and **Merge it**. `dot-agent-deck card catalog` prints the catalog the daemon uses. Earlier releases also had an **Explain** action on every card; to keep it, add it to your catalog as the example below shows. A `remove = ["explain"]` you wrote to hide it still works and needs no change; the deck ignores it without a warning.

### The Card actions editor

Each daemon's header on the Dashboard has a **⋯** button just left of **New agent**. Choose **Card actions** in its menu to add, change, hide or remove the actions that daemon's agents can offer, without editing a file. Two shortcuts open it too: **Edit card actions** in the selection bar, for the daemon of the selected cards (see [Send one action to many agents](#send-one-action-to-many-agents)), and **Edit** beside **Actions** in an agent's Card tab, for that agent's daemon. The editor's title names the daemon, such as **Card actions: Local daemon**. It lists every action of the catalog with:

- its icon, label, the exact text it sends and when agents should offer it;
- where it came from: **deck default**, **deck default, your version** for a default you changed, or **yours**; a default you hid is listed too, marked **hidden**;
- what you can do with it: **Edit** on every action; **Remove** on your own; **Hide** on a deck default, which stops agents offering it; **Restore** on a hidden or changed default, which brings it back as the deck ships it.

**Add action** and **Edit** open a form with **Name** (lower-case letters, digits and dashes; it cannot change once the action exists), **Label**, **Text it sends** (sent exactly as written, line breaks included), **Tell agents when to offer it** (agents read it to decide when to put the action on their card; it is not shown on the card), an **Icon** chosen with **Choose icon** from a grid you can search, **Placement** (**Show on the card**, the default, draws the action as an icon on the cards that offer it; **Only in the Card tab** lists it only when you open a card), **Ask before sending**, and **Offer on every card**. **Save** writes the action; while it is saved the form cannot be changed and no other form opens. If the daemon refuses it, the form says why and keeps what you typed. If the daemon's answer never arrives, the change may or may not have been saved: the editor says so, keeps your form, and reads the list again so you can see which, saying when it has, or that it could not, in which case press **Refresh** to try again. **Refresh** reads the list again whenever you want, keeping the form. **Remove** asks first, naming the action. A change reaches the cards at once, with no restart.

The editor edits the catalog of the daemon whose **⋯** you opened it from: that daemon's own user configuration file, on its own machine. It shows where that file is. To change another daemon's actions, close the editor and open **Card actions** from that daemon's **⋯**. If the editor cannot read a daemon's actions, it says why and lists none, rather than showing the deck's defaults as if they were that daemon's; when the daemon reads its file but finds problems in it, the editor lists them above the actions. Other settings, the catalog's fields and the comments in the file are kept as they are, including a comment on a setting the form leaves at its default. A file the daemon cannot read is never overwritten: the editor says what is wrong, and you fix the file by hand.

A save is refused, with nothing written, when:

- the action you are changing is no longer what the editor showed you, because another program, another editor or you changed it in the file since the editor read it; press **Refresh**, check the action, and save again (the form keeps what you typed);
- the daemon finds that the file changed while it was preparing the save, for example because you saved it by hand at that moment; press **Refresh** and make the change again. While the daemon saves, `dot-agent-deck config set` waits for it, and it waits for `config set`, so the two do not overwrite each other; a save that waits more than a few seconds is refused and can be tried again. The same holds for `config set`: it waits briefly for the editor's save, and if that keeps it waiting it saves nothing, says so and exits with an error, and you run it again;
- the lock file beside the configuration file, `.dot-agent-deck-config.lock`, is not safe to use: the editor and `config set` both refuse and name it, along with the reason. The daemon and `config set` make that file themselves, readable only by you; one that another user made, that others can read or write, that has another hard link, or that is not a regular file (a link, for example) is not used. Remove it if you did not make it, or ask whoever owns it to, then try again;
- a folder on the path to the file, including a folder that holds a link on that path, is one that another user can change, so the file could be swapped while it is saved. The message names the folder and the reason; follow it, or keep the configuration in folders only you can change;
- the file cannot be replaced in one step: it has another hard link, or its folder does not let the daemon create a file beside it. Edit the file by hand, or move it somewhere the daemon can replace it;
- the configuration path is a link whose target is missing, or is not a regular file.

A daemon from an earlier release cannot be edited from the app: the editor says so and offers no changes for it. Upgrade that daemon, or edit its file as below.

The editor changes actions only. The fields agents fill in are changed in the file.

### In the file

The catalog lives in a `[catalog]` table in your user configuration file, `~/.config/dot-agent-deck/config.toml` (or the file `DOT_AGENT_DECK_CONFIG` names). That is what the editor writes, and you can edit it yourself: it is the only way to change the fields, and the way to change actions from the TUI, which has no cards view. The daemon reads that file, so for a daemon on another machine it is the file on that machine. Edits apply to the next card read or publish, with no restart.

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
icon = "rocket"                    # optional: one of the icons the editor's grid shows
placement = "card"                 # optional: card (the default, an icon on the card) or detail (only in the Card tab)
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
- The desktop app shows every field in the Card tab, whatever its `placement`; `dot-agent-deck card list` reports each field's placement.
- An entry with a mistake (an unknown type, a missing description, a bad name) is skipped and the rest still apply. `dot-agent-deck card catalog` prints a warning for each one.
- A mistake elsewhere in the file that makes it unreadable, such as a missing quote in another setting, does not cost you the catalog: the deck reads the `[catalog]` tables on their own, uses them, and warns that the file needs fixing. A mistake inside the `[catalog]` tables themselves leaves the defaults in use, with a warning. `dot-agent-deck config set` keeps your `[catalog]` and its comments, and refuses to save over a file it cannot read.
- The actions **Reply**, **Open**, **Close** and **Close, keep worktree** are the deck's own and cannot be removed or changed.
- A `[catalog]` table in a project's `.dot-agent-deck.toml` is not used yet. Only the user configuration file's applies.
- Changes apply to cards agents already published. When you remove an action from the catalog, it disappears from the cards that suggested it only by its catalog name; a card whose agent gave that action its own label and text keeps offering it, marked custom. When you change a field's type, a value an agent already reported is hidden if it does not fit the new type, until the agent reports it again; a value that still fits, such as text in a field you changed to long text, stays.

Agents can also report fields and actions that are not in the catalog. You see them marked **custom** in the Card tab, and an action the agent suggested says so when you hover its icon.

## In the TUI

The TUI does not show cards, and it does not draw the tree; its agent pane has no Card tab. Its dashboard keeps showing each agent's status as before. Opening an agent's pane in the TUI (`Enter` on its card, or its number) counts as opening it, so it clears the same **Needs you** in `card list` and in the desktop app. From any shell on the machine, `dot-agent-deck card list` prints every card as JSON, with the same status, needs-you reasons and actions the app shows, and `dot-agent-deck card act` presses one action. Sending one action to many agents at once is done in the desktop app; from a shell, `card act` acts on one card at a time. The **Card actions** editor is in the desktop app too; in the TUI, change card actions in the `[catalog]` table of the user configuration file (see [In the file](#in-the-file)). Each card in the list also carries the tree the app draws: `parent` names the key of the card that started it (a card with no `parent` is in the top row), `kind` says whether it is a dispatcher, orchestrator, role or agent, `rollup` counts the agents under it by status with the most urgent one and how many need you, and `checked_refs` lists the pull requests and issues the agent reported, each with what the agent wrote and what the deck found, including an issue's `labels`. The cards are listed in the tree's order, each one before the agents it started.

```sh
dot-agent-deck card act --entry <key> --action <id> --token <token> --instance <instance_id>
```

Take every value from `card list`: the card's `key`, the action's `id` and `token`, and the list's `instance_id`. The token says which version of the action you read, so if the agent changed the action since, nothing is sent and the command says so; list the cards again. Add `--confirm` for an action that asks for confirmation and `--text` for **Reply**. Run `card act` from your own shell, not from an agent's pane: the deck refuses it there, so that one agent does not press another agent's buttons by mistake.
