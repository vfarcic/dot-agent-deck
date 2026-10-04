# Voice Control

The desktop app can be driven by voice: open screens and agents, switch daemons, fill in and submit the New agent dialog, stop agents (after you confirm on screen), type or dictate into an agent, and answer the question an agent is waiting on. The TUI has no voice control.

Voice needs two services, set in **Settings → Voice**: **Speech** turns your audio into text, and **Commands** turns that text into one of the app's commands. Read [What is sent where](#what-is-sent-where) before turning it on. Voice understands English: what you say is always transcribed as English, whatever your accent.

## Turn on voice control

With the default settings, speech runs in a container on your machine and commands go to OpenAI, so you need Docker and an OpenAI API key.

1. **Start the local speech service.** It listens on port 18000 by default:

   ```bash
   docker run -d -p 18000:8000 ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu
   ```

   The first thing you say downloads its speech model, so the first answer takes longer.
2. **Store the Commands key.** Open **Settings → Voice**. Under **Commands** (default **OpenAI-compatible API**), paste your OpenAI API key into **Commands key for api.openai.com** and press **Save**. The panel then reads "A key is stored in your OS keychain."
3. **Turn voice on.** Press **Voice off** at the bottom of the window. It changes to **Voice on**, and the app listens continuously until you turn it off. Your operating system may ask the first time whether the app may use the microphone; allow it.
4. **Check it worked:** say "what can I say?". A list of every command opens, split into **On this screen** and **On another screen**. Close it with **Close**.

To turn voice off, press the button again or say "voice off". The microphone is released when voice is off. Voice is off whenever the app starts.

To avoid the Docker step, choose **OpenAI — needs an OpenAI API key** under **Speech** and store a Speech key; your audio then goes to OpenAI. To avoid an OpenAI key for Commands, choose **Anthropic API** and store an Anthropic key, or point **Endpoint** at an OpenAI-compatible server on this machine, which needs no key.

## Using it

What the app did with each thing you said appears beside the Voice button and stays until the next one. The app keeps listening while it works out what you just said, so you can go on talking. You can pause in the middle of a command: when what you have said so far does not make a whole command, such as "set the command to be", the app waits about two seconds for the rest before saying it found nothing, and joins what you say next onto it. A command that is already complete runs without that wait. While it works out what you meant, the row shows the words it heard, for example *Heard “open settings”*; the answer then replaces them, and when the answer quotes your words itself (*Heard: “…” — no matching action.*) they are shown only once. When a command can be reversed, an **Undo** button is shown beside it for ten seconds. While voice is on, the text in that row at the bottom of the window is larger, so you can read it from where you sit, and a long message wraps onto more lines instead of being cut off; it returns to its usual size when you turn voice off.

Sounds that are not speech, such as typing on the keyboard, a breath, a cough, a knock on the desk, a notification sound or other noise in the room, are ignored: nothing is typed or run for them, and what is shown beside the Voice button stays as it was. Speech from somewhere else is still speech, though: another person talking, or a television or video playing near the microphone, is heard as if you had said it.

What you can do by voice, by screen:

| Where | You can |
| --- | --- |
| Anywhere, including an agent's pane | Close what is open over the screen (a pane, Settings, a dialog); list the commands; turn voice off |
| Dashboard | Open Settings; switch which daemon the app shows; open an agent's pane; open New agent; stop an agent or close an orchestration (the app shows the same confirmation as the buttons, and nothing stops until you confirm by hand) |
| New agent dialog | Choose the daemon; open a directory, go up, use the directory shown; [filter the directories](#filtering-directories) or clear the filter; choose a Mode chip or the agent to run; set the Name; [set the Command](#setting-the-command); start; discard |
| An agent's pane | Type into the agent's prompt; submit it; turn [typing mode](#typing-mode) on and off; [answer the question the agent is waiting on](#answering-an-agents-question) |

While voice is on, the agents, daemons, directories and modes on screen are numbered, and saying a number chooses one; see [Choosing by number](#choosing-by-number). A list too long for the window is shown a page at a time, so everything you can choose is on screen; see [Long lists are shown in pages](#long-lists-are-shown-in-pages). When what you said matches more than one thing on screen, the app lists the matches for you to choose from; see [When a command matches several things](#when-a-command-matches-several-things).

A command that names an agent, such as "stop the planner" or "close the review orchestration", runs nothing, and the app says why, if you switch daemons or that agent is replaced by a new one under the same name while the app is still working out what you said (which the app can tell only when the daemon reports when each agent started). Say it again to act on the agent that is there now.

**Typing into an agent.** Open its [pane](dashboard.md#the-agent-pane) and start with "type", "write", "say" or "dictate", for example "type run the tests". The words after the opener are typed into the agent's prompt, taken from what the Speech service heard rather than from the Commands model's answer. Other phrasings ("tell it to …") work too; the Commands model finds where your words start, and the app checks that they really are your words before typing them. After typing, a five-second countdown runs on screen and then the prompt is sent. Speaking again stops the countdown; another "type …" adds its words to the prompt and starts the countdown again, and otherwise the prompt waits for you to send it. To send, say "send", "send it", "submit", "enter", "press enter" or "end" as the whole utterance, or press `Enter` yourself. To dictate a longer prompt without starting every sentence with "type", use [typing mode](#typing-mode).

Nothing is typed or sent, and the app says why, if a confirmation is open, if the pane is showing another tab instead of the agent's terminal, or if the pane on screen is no longer the one you were looking at when you spoke — for example because you opened another agent while the app was still working out what you said. Once the words are typed, the send is called off, and the app says so, if before it happens you close the pane, open another screen or agent, switch daemons, or a confirmation opens: the words stay in the agent's prompt, unsent. It is also called off if the agent in the pane is replaced by a new one. Telling a replacement apart needs a daemon that reports when each agent started; with one that does not, the app cannot tell a replacement from the original agent.

**While voice is on, the app keeps the computer from going to sleep from inactivity**, because speaking produces no keyboard or mouse input. The display can still turn off. On Linux this goes through systemd-logind; a desktop environment whose power manager ignores logind may still suspend.

## Answering an agent's question

When an agent stops to ask you something — a permission prompt such as "Allow this command?", a menu of options, or a form of several questions — its card shows **Needs Input** (see [Session statuses](../session-management.md#session-statuses)). Open that agent's [pane](dashboard.md#the-agent-pane) and answer it by voice, saying what you would say to a person:

- **A permission prompt:** "yes", "go ahead" or "allow it" allows it once; "no" or "don't" denies it; "always allow" chooses the agent's "always" option, after you confirm it (below).
- **A menu:** the option's number or position, such as "option 2" or "the second one", or its words, such as "blue".
- **A form of several questions:** answer them in any order, in one sentence or several, such as "red for colour and large for size". On a question that takes several answers, name them together: "small and large". Answering a question again replaces your earlier answer. The row at the bottom of the window shows the form filling in, for example *So far — Colour: Red. Still to answer: Sizes.*
- **An option that asks for your own words**, such as Claude Code's "Type something.": choose it, and the row asks you to *Say the words for “Type something.”*; what you say next is taken as your answer, word for word.

![An agent's pane with Codex asking whether to run npm install --save-dev msw, its three options listed in the pane, and at the bottom of the window the voice row after "yes" was heard: Allow once — npm install --save-dev msw — sending in 5 s, with a Cancel button](/img/voice-question-desktop.png)

Once every question has an answer, the row shows it with a five-second countdown, for example *Allow once — touch notes.txt — sending in 5 s. Say “cancel” to stop.* When the app hears you start speaking, the countdown stops — *stopped because you spoke. Say the answer again to send it.* — before it works out what you said, and nothing is sent until you give a complete answer again, which starts a new countdown. Speech that starts in the very last moment may come too late to stop it. Press **Cancel** to call it off without speaking. When the countdown ends, the row shows *sending…* until the deck has answered the agent, and **Cancel** still works until then; then the row says what it sent: *Allowed: touch notes.txt*, *Denied: touch notes.txt*, or *Answered: Colour → Red; Sizes → Small, Large*. If you cancel after the answer already reached the agent, the row says so: *Too late to cancel — Allowed: touch notes.txt*. Always read the countdown: the app picks the option it thinks your words chose, and it can pick wrongly — for example "Allow once" for something you said about something else. What the countdown shows is exactly what will be sent, and speaking or **Cancel** stops it.

**Always allow.** Choosing an "always" option opens a confirmation, **Always allow?**, that says what it will always allow, for example *This will always allow access to /work/proj for the rest of this session. Confirm?* Press **Confirm**, or say "confirm" or "always allow" again, and the countdown starts. "Yes", "sure" or "OK" does not confirm it: the confirmation stays open and the row says *Say “confirm” to always allow, or “cancel”.* Press **Cancel**, close the confirmation, or say "cancel" or "no", and nothing is sent. The deck grants exactly what the confirmation names; when it cannot say what an agent's "always" option would grant, or the grant is too long to show in full, that option has to be chosen in the pane.

Anything you say that is not about the question, such as "open settings", is handled as an ordinary command. While [typing mode](#typing-mode) is on, what you say is typed into the prompt instead: say "type off" first. You can answer only the agent whose pane is on screen with its terminal showing — not from the dashboard, and not while the pane shows another tab such as its diff — so that you can see what you are approving; there, what you say is an ordinary command. Answering in the pane with the keyboard works as it always has.

Nothing is sent, and the row says why, when:

- the question was answered, for example with the keyboard, or changed before the answer went: *That question was answered or changed before I could send it — nothing was sent.*
- you close the pane, open another agent or screen, switch the pane to another tab, switch daemons, turn voice off, a confirmation opens, or the agent in the pane is replaced, while the countdown runs or the answer is being sent;
- what you said does not match an option: *I couldn't match that to the options: …*, listing what you can say;
- the app could not tie the option it heard to the words you said: *Heard: “…” — I couldn't tell which option that chooses, so nothing was chosen.* Say the answer again, for example with the option's number;
- you have already started answering in the pane with the keyboard, on an agent the deck answers by pressing keys (Codex, Devin): *… 's prompt was typed into after it asked, so the deck won't type the answer there — finish it by keyboard.* The deck does not press keys into a prompt you may have moved on, and if it had started a form when you typed, it stops and leaves the rest to you;
- the option you chose cannot be answered by voice: *“Chat about this” has to be answered by keyboard.*
- the agent's questions cannot be answered by voice at all: *… 's questions have to be answered by keyboard.*
- the daemon is older than the app and cannot answer questions: *This deck cannot answer questions by voice — update the deck to use it. Nothing was sent.*

### Which questions each agent can answer by voice

| Agent | By voice | By keyboard only |
| --- | --- | --- |
| Claude Code 2.1.136 or newer | Permission prompts: **Yes**, the "always" option, **No**. Multiple-choice questions and forms, including questions with several answers and **Type something.** Plan approval: **Yes, auto-accept edits** and **Yes, manually approve edits**. | **Chat about this**; plan approval's **Tell Claude what to change**; forms from MCP servers; the folder-trust question when Claude Code starts. |
| Codex | Command approvals: **Yes, proceed**, the "don't ask again" option, **No**. Multiple-choice questions and forms in Plan mode, one answer per question. | **None of the above**; the folder-trust question when Codex starts. |
| OpenCode | Permission prompts: **Allow once**, **Allow always**, **Reject**. Question menus and forms, including questions with several answers and **Type your own answer**. | — |
| Pi | Pi asks no questions of its own. The dialogs other Pi extensions show: a choice from a list, a yes-or-no confirmation, and a box to type into. | Other extension dialogs, such as an editor or a custom screen. |
| Devin | Permission prompts: **Allow once** and deny. **Untested**: built from Devin's documentation and not yet tried with a running Devin. | Every other permission option. |

- **Claude Code older than 2.1.136:** the deck does not know its questions. The card still shows **Needs Input**; answer in the pane. After upgrading Claude Code, run `dot-agent-deck hooks install` (or restart the daemon), then restart Claude Code.
- **Codex:** its questions reach the deck through the deck's hooks, which have to be installed and trusted ([Codex events not showing](../troubleshooting.md#codex-events-not-showing)). This version of the deck adds one more Codex hook; it is installed and trusted with the others when the daemon starts or when you run `dot-agent-deck hooks install --agent codex`. Restart Codex sessions that were already running.
- **Pi:** answering rests on behaviour Pi does not document, checked with Pi 0.87.1. A later Pi release may stop it working; its dialogs then have to be answered in the pane.
- **The labels the app shows for Claude Code's and Codex's permission prompts, and the keys the deck presses to answer Codex,** were checked against Claude Code 2.1.289 and Codex 0.160.0. If a newer release changes those prompts, check the pane before the countdown ends.

## Filtering directories

While the New agent dialog shows a directory, say "filter" followed by what to look for, such as "filter docs" or "filter by api". The app puts that text in the **Filter** box, exactly as if you had typed it, and the list shows only the directories whose names contain it, in upper or lower case. Longer requests work too: "show only those starting with letter D" sets the filter to `d`, which, as when you type it, also keeps names that have a `d` anywhere else. The app says what it applied, for example *Filtering by “d”.* It uses only words you said: if what you asked for is not in your words, the filter is left as it was and the app says so. Say "clear filter" to empty the box and show every directory again.

## Setting the command

Once the New agent dialog has a daemon and a directory chosen, say "set the command to" followed by the command, such as "set the command to devbox run agent" or "make the command npm run dev". The app puts exactly the words you said in the **Command** field, as if you had typed them, and says what it set, for example *Command: “devbox run agent”.* Only the full stop at the end of your sentence and any quotes around the command are left out. If what would go in the field is not what you said, the field is left as it was and the app says so. The command must be what you said word for word, so "set the command to ./run.sh" sets `./run.sh`, never `run.sh`, and a command containing an invisible character, such as a direction override or a zero-width space, is refused. Setting the command starts nothing, even when the command contains words like "run", and a sentence that mentions the command never starts the agent, such as "set the command to bash and start it": check the form, then say "start it" on its own or press **Create agent**. Saying "use claude" (or another agent) still fills in that agent's usual command instead.

## Choosing by number

While voice is on, the lists you can choose from by voice show a number before each item: the agents on the dashboard, the agent tiles on the Daemons screen, the daemons, directories and modes in the New agent dialog, and the daemons in the **Daemon** selector while its menu is open. With voice off, they look as they always do.

Each list is numbered from 1. When several lists are visible together, as in the New agent dialog, each has its own numbers: the daemons 1, 2…, the directories 1, 2… (with `..` for the folder above as 1), and the modes 1, 2…. On the dashboard the agents of every daemon are one list, numbered top to bottom. While an agent's pane, the New agent dialog or the **Daemon** menu is open, only what is in front is numbered. A list shown in pages is numbered from 1 on every page; see [Long lists are shown in pages](#long-lists-are-shown-in-pages).

![The agent dashboard with voice on: each agent row starts with a number, 1 to 6, counting on from one daemon's agents to the next](/img/voice-numbers-desktop.png)

- **Say the list and the number**: "directory 3", "select directory 13", "daemon 1", "choose mode 2", "agent 3" or "open agent 3". "Folder" works for directory, "deck" for daemon, and you can start with "select", "choose", "pick", "open", "enter", "go to" or "switch to". The item showing that number in that list is chosen as if you had clicked it: an agent opens, a daemon or mode is chosen, a directory opens (`..` goes up).
- **Say the number on its own**: "three", "3", "number three", "the third one" or "the last one". When only one list on screen shows that number, its item is chosen straight away. When several do, for example directory 3 and mode 3, the app asks which one you meant, listing them as "Directory 3: scratch" and "Mode 3: Review".
- Numbers above nine work the same way, in words or digits: "twelve", "number twenty-three", "the twelfth", "23".
- **Press the number key**, such as `3`, on a list that shows numbers. It chooses from the list you are in: with the directories selected, `3` opens directory 3; with a mode selected, `3` chooses mode 3. On the Daemons screen the `1`–`4` keys already select the tile with that number. A digit typed into a text field, such as the directory **Filter**, **Name** or **Command**, is just typed.

The numbers follow the list. When it changes, for example a directory opens, you filter it or an agent finishes, the numbers change straight away. If the list changed while you were saying a number, nothing is chosen and the app says so, because the number may now belong to another item; say it again. A number that no item shows chooses nothing, and the app says so. Naming the wrong list chooses nothing either: "select daemon 13" while only a directory shows 13 leaves everything as it is, and the app tells you that 13 is a directory. The Mode chips can be chosen by number once a directory is chosen; until then a number names only a daemon or a directory.

If the number you said could also be an item's name, for example "one" with an agent called `orchestrator-1`, or "folder 13" with a directory called `folder-13` that shows another number, the app does not guess: it asks which one you meant, as described below. "Directory 13" always means the directory showing 13.

A spoken number, with or without its list, is worked out on your computer and is not sent to the Commands service. Anything else, such as "open number three", is a command like any other.

## Long lists are shown in pages

While voice is on, a list that does not fit in the window is shown a page at a time instead of scrolling, so everything you can choose by voice is on screen. This applies to the directories and the modes in the New agent dialog, the agents on the dashboard and the agent tiles on the Daemons screen. A list that fits is shown whole, as before. With voice off, every list scrolls as it always has. The daemons in the New agent dialog are never split into pages: every daemon that can take a new agent is always shown in full.

![The New agent dialog with voice on: the daemons, the directories and the modes are each numbered from 1, the directories fill the dialog in five columns, and "Page 1 of 3" shows beside the Directory heading](/img/voice-pages-desktop.png)

- **Each page fills the space the list has.** Directories and modes are laid out in as many columns as fit, and a short list uses only the columns it needs, so a few long directory names are shown whole. A name that is cut short shows in full when you hover over it. The dashboard shows as many agents as fit the window and works the pages out again when you resize it. The Daemons screen shows four tiles per page, the four the `1`–`4` keys select.
- **"Page 2 of 3"** shows beside a list that has pages: next to the **Directory** heading or under **Mode** in the New agent dialog, next to the dashboard's title, and above the tiles on the Daemons screen.
- **Say "next page" or "previous page"** to turn it ("go to the next page" and "go back a page" work too). On the last page, the first page, or a screen with no pages, nothing moves and the app says why. In the New agent dialog, the directories turn until you choose one, and then the modes do.
- **Numbers start at 1 on every page** and refer to the page showing. Turning the page changes the numbers, so a number you began to say before the page turned chooses nothing and the app asks you to say it again.
- **Voice works only on the page showing.** Naming something on another page, such as "open docs", chooses nothing; the app tells you where it is, for example *“docs” is on page 3: say “next page”*. Clicking, the keyboard and the **Filter** box work as usual: moving the selection with the arrow keys or `j` and `k` turns to the page it lands on.

## When a command matches several things

If what you said names more than one thing on screen, for example "open the agent" with several agents on the dashboard, the app does not guess. A dialog opens in the middle of the screen, over whatever is there (an agent's pane or the New agent dialog included), asking which one you meant. It lists the matches as a numbered list, with a **Cancel** button and a countdown, and waits for you to choose. The row at the bottom keeps showing what the app heard.

![The agent dashboard with voice on and "open the agent" heard: a dialog in the middle of the screen asks "Which agent?", lists 1. Plan / architecture and 2. Desktop implementation, and shows a 20 s countdown and a Cancel button, while the row at the bottom says what was heard](/img/voice-choice-desktop.png)

- **Say the number**: "two", "2", "number two", "option two", "the second one" or "the last one", on its own.
- **Say the name** of one of the listed entries on its own, such as "Desktop implementation". A sentence that only contains a listed name, such as "stop Planner", is not an answer: the list closes and that sentence runs as a new command.
- **Press its number** on the keyboard, such as `2`. A number that is not on the list does nothing.
- **Click** an entry, or reach it with `Tab` and press `Enter`.

The dialog takes the keyboard while it is open, starting on the first entry, so the keys you press go to it rather than to an agent's terminal. When it closes, the keyboard goes back to where it was.

If an entry's name is itself a number or a way of cancelling, for example an agent called "two" or "cancel", saying just that name chooses nothing: the list closes and the app says why. Say the command again, then say "number" and the entry's position, such as "number one", or click the entry.

The command you first gave then runs with the entry you chose; what you said is not sent to the Commands service again. Choosing does not skip a confirmation: if the command stops an agent or closes an orchestration, the confirmation still opens, and you answer it by hand.

To choose nothing, say "cancel", "cancel that", "never mind", "none", "none of them", "neither" or "no" on its own, press **Cancel**, press `Escape`, or click outside the dialog. `Escape` closes only the dialog: a pane or the New agent dialog behind it stays open. The dialog also closes on its own when its countdown reaches zero after 20 seconds, and when you turn voice off; nothing runs.

If you say something else while the list is open, the list closes and what you said is treated as a new command. A number that is not on the list, a name that matches more than one entry, or on its own the name of something on screen that is not on the list, closes the list without running anything; say the command again, more specifically.

Nothing runs, and the app says why, if what the list was about changed after it appeared: you moved to another screen, the New agent dialog opened, closed or changed, the directory it showed moved, a daemon's address changed, or the agent, daemon or orchestration you chose is no longer there. The same goes if you switch daemons, or if the agent you chose was replaced by a new one under the same name after you gave the command (which the app can tell only when the daemon reports when each agent started).

A list is not offered while a confirmation is open, or when more than nine things match; the app then says what matched, and you say the command again, more specifically. Some refusals are never turned into a list, for example "switch to build, not staging", where choosing between the two would let you pick the daemon you just ruled out.

## Typing mode

When you want to dictate a longer prompt, you do not have to start every sentence with "type". Open the agent's pane and say "type on" (or "typing on", "start typing", "dictation on", "start dictation", "keep typing"). From then on, everything you say is typed into that agent's prompt, word for word, until you stop.

While typing mode is on:

![An agent's pane with typing mode on: "Typing to Desktop implementation" on the pane's top edge, and at the bottom of the window the Voice on button, a Stop typing button and the reminder "Typing to Desktop implementation. Say “type off” to stop, “send it” to send."](/img/voice-typing-mode-desktop.png)

- **Typing to** *agent* is shown at the top of the agent's pane and beside the Voice button, with a reminder of what to say to stop and to send, and a **Stop typing** button.
- Nothing is sent to the agent until you say "send it" (or "send", "submit", "enter", "press enter", "go ahead", "finished", "end") on its own, or press `Enter` yourself. After a send, typing mode stays on, so you can dictate the next prompt the same way.
- You can also end what you say with "send it", "send", "submit" or "press enter" as a separate last sentence, for example "What's the weather over there? Send it." Everything before it is typed and then sent; the last sentence itself is not typed. The other send phrases ("enter", "go ahead", "finished", "end") send only when said on their own, so "Fix the tests. Go ahead." is typed in full and not sent.
- If you stop talking for four seconds with words typed but not yet sent, “send it” to send is highlighted as a reminder. It never sends anything itself; the highlight goes away when you speak again, send, or stop typing.
- Only a handful of things you can say still act as commands, and only when you say them on their own (or, for the four send phrases above, as a separate last sentence): "type off" (or "typing off", "stop typing", "dictation off", "stop dictation", "done typing"), the send phrases above, and "voice off" (or "mute", "mic off", "stop listening", "stop voice"). Said inside a longer sentence, such as "tell the reviewer to send it when the tests pass", they are typed like anything else. Other commands, such as opening a screen, do not work until you stop typing.
- If you speak for 30 seconds without a pause, what you said is still typed, and the app tells you it reached the limit.

Typing mode ends, and **nothing is sent** when it does, if you:

- say "type off", or say "voice off", which also turns voice off;
- press **Stop typing** (you can also reach it with `Tab`), which leaves voice on, or press the Voice button, which turns voice off;
- close the pane (for example with `Escape`), switch the pane to another tab such as **Diff** or **Checks**, open another screen, open another agent, or switch daemons;
- or a confirmation opens, for example to stop an agent.

It also ends if the agent stops accepting input, for example because it exited, or if it is replaced by a new agent in the same pane (which the app can tell only when the daemon reports when each agent started). Something you said just before typing mode ended that the app was still working out is not typed or sent, but words it had already handed to the agent can still appear in the prompt after typing mode ends; they are not sent. If the agent you were typing to is still there, whatever you dictated stays in its prompt, where you can edit it or send it yourself; a replacement agent starts with an empty prompt. The app says why typing mode ended. Typing mode never moves to another agent on its own: to dictate to a different agent, open its pane and say "type on" again. If the agent on screen cannot take input, "type on" is refused with the reason.

If the app mishears "type off", the words are typed into the prompt instead of stopping typing mode. They are not sent: press **Stop typing** and delete them.

A bare "type on" or "type off" always switches typing mode, so it cannot be used to type the single word "on" or "off". Say "type the word on" instead.

## Settings → Voice

![Settings → Voice with the default services: Speech on this machine, through a local speech container, and Commands through an OpenAI-compatible API, each with its Endpoint and Model, then what Commands sends and Names set to Shared](/img/settings-voice-desktop.png)

| Row | What it sets | Default |
| --- | --- | --- |
| **Speech** | Where speech is turned into text. **On this machine — speech container, no key** uses a local container and sends no key; its endpoint must be on this machine. **OpenAI — needs an OpenAI API key** sends your audio to OpenAI. | On this machine |
| **Commands** | Where the text is turned into a command. **OpenAI-compatible API — needs that provider's API key**, or **Anthropic API — needs an Anthropic API key**. | OpenAI-compatible API |
| **Endpoint**, **Model** | The URL and model of each service. Choosing a service fills in its usual values; change them to use another provider or a local server. An endpoint must be `https`, or `http` to this machine (`localhost`, `127.x.x.x`, `::1`). | See below |
| **Max tokens** | The longest answer the Commands model may give, 64 to 32768. A model that reasons spends part of this on its reasoning; raise it if answers come back cut off. | 4096 |
| **Names** | **Shared** or **Withheld** (below). | Shared |
| **Speech key for** / **Commands key for** *host* | The API key for a service that needs one: paste it and press **Save**, then **Replace** or **Forget** it later. Shown for the OpenAI speech service, and for a Commands endpoint that is not on this machine. | None |

The values each choice fills in:

| Choice | Endpoint | Model |
| --- | --- | --- |
| Speech, on this machine | `http://127.0.0.1:18000/v1/audio/transcriptions` | `Systran/faster-whisper-tiny.en` |
| Speech, OpenAI | `https://api.openai.com/v1/audio/transcriptions` | `whisper-1` |
| Commands, OpenAI-compatible API | `https://api.openai.com/v1/chat/completions` | `gpt-5-mini` |
| Commands, Anthropic API | `https://api.anthropic.com/v1/messages` | `claude-haiku-4-5` |

These are stored in the `[voice]` table of the [settings file](settings.md#the-settings-file): `[voice.transcription]` with `backend` (`"local"` or `"remote"`), `endpoint` and `model`; `[voice.intent]` with `backend` (`"openai_compatible"` or `"anthropic"`), `endpoint`, `model` and `max_tokens`; and `labels` (`"shared"` or `"withheld"`). An endpoint or model left out takes the chosen backend's value above. A `"local"` speech backend with an endpoint off this machine is refused, and the app then treats the whole file as unreadable.

Keys are stored in your operating system's credential store (the macOS Keychain, or the Secret Service on Linux), not in the settings file: one key for **Speech** and one for **Commands**. The panel says whether a key is stored, or that the credential store could not be reached.

## What is sent where

- **To the Speech service:** your audio, the model name and the language (English). With the default local container, it stays on this machine. The OpenAI speech option also sends your Speech key.
- **To the Commands service, for each utterance it decides:** the words it heard, the app's fixed instructions and answer format, the model name and token limit, and the app's list of commands (each command's id, description, parameter names and kinds, whether it can run on the current screen, and the hint shown when it cannot). When the endpoint is not on this machine, the request also carries your Commands API key.
- **To the Commands service, while the pane on screen has a [question waiting](#answering-an-agents-question):** what you said and the question itself — each question's text and heading, each option's words, description and what an "always" option covers, the tool the question is about with its detail (for example the command the agent wants to run), and the answer you have given so far. This is sent whether **Names** is shared or withheld, because the question cannot be answered without it.
- **Decided on this machine, sending nothing:** while a question is waiting, an utterance that is exactly one option's words, a position on a one-question menu ("two", "option two"), the words for an option that asks for your own words, or "cancel" or "confirm" while the answer's countdown or confirmation is showing; an utterance that starts with the word "type", "write", "say" or "dictate" followed by words to type; one that is, in its entirety, "end", "send", "send it", "submit", "enter" or "press enter" (case, punctuation and a word such as "okay" or "please" before or after it ignored); while the New agent dialog is open, one that is in its entirety a way of closing it, such as "close", "cancel" or "close new agent"; while a [numbered list](#when-a-command-matches-several-things) is open, a number, a listed name or a way of cancelling it; while the lists on screen show [numbers](#choosing-by-number), a number on its own or after the list's name, such as "three" or "select directory 13"; and "type on" and "type off" themselves. While [typing mode](#typing-mode) is on, nothing you say is sent to the Commands service at all: it is typed into the agent or, for the few phrases that still work, handled on this machine. Everything else goes to the Commands service, including other ways of saying submit such as "go ahead". Silence sends nothing.
- **With Names shared**, each request also sends the names on screen: each agent on the selected daemon with its name, role, CLI name, status and running tool; each daemon's name, and for a remote daemon with no name its ssh user, host and any non-default port instead; while the New agent dialog shows a directory, up to 200 directory names from it and whether it has a parent; the dialog's Mode chips (including the project's orchestration names) and agent entries; and each orchestration's title and roles. The app adds no filesystem path, id, prompt text or tool argument of its own, but a name is whatever it was set to, and can itself be a path.
- **With Names withheld**, none of those names is sent, so the commands that name an agent, daemon, directory, mode, agent type or orchestration are unavailable. Your words are still sent as heard.

The panel states the same thing beside the **Commands** setting.

## When it does not work

What went wrong is shown beside the Voice button.

| What you see | Cause | What to do |
| --- | --- | --- |
| "nothing is listening at http://127.0.0.1:18000 — start the speech service with `docker run …`" | The local speech container is not running, or listens on another port. | Run the `docker run` command it shows, then press **Voice** again. |
| "no key is stored for *host* — add one in Settings → Voice" | The service needs a key and none is stored. | Store one in **Settings → Voice**. |
| The credential store could not be reached | No keychain or Secret Service is available (for example a Linux session with no Secret Service provider), or it is locked. | Unlock it, or install and start a Secret Service provider such as GNOME Keyring; or use services on this machine, which need no key. |
| A command is refused as not available here | It works on another screen. | Say "what can I say?" to see where each command works. |
| Commands that name agents or directories never work | **Names** is set to **Withheld**. | Set it to **Shared**, or use the screen instead. |
| A command runs nothing and says the agent, daemon, screen or list changed | What it was about changed while the app was working out what you said, or after a numbered list appeared. | Say the command again. |
| "yes" to an agent showing **Needs Input** finds no matching action | The deck does not know that question: the agent or its version is not covered, or its hooks are missing. | Check [which questions each agent can answer](#which-questions-each-agent-can-answer-by-voice), and answer in the pane. |
| An answer to a question is refused with "has to be answered by keyboard" | That option, or that agent's questions, cannot be answered by voice. | Answer in the pane. |
| A spoken number chooses nothing and says the numbers on screen changed | The list changed while you were saying it. | Look at the new numbers and say it again. |
| "Select daemon 13" chooses nothing and says no daemon shows 13 | The number belongs to another list, such as the directories. | Say that list's name with the number, for example "directory 13". |
| A name is refused with "is on page 2: say “next page”" | The item is on another page of a list shown in pages. | Turn to that page and say it again, or narrow a directory list with "filter …". |
| "type off" was typed into the prompt instead of stopping typing mode | The app misheard it. | Nothing was sent: press **Stop typing** and delete the words. |
| A command you paused in the middle of was answered in two halves | You paused for longer than the app waits for the rest, which is a few seconds. | Say the whole command again, with a shorter pause. |
| Words you did not say were typed into the prompt in typing mode | Speech from somewhere else, such as another person or a video playing, was heard as yours. | Nothing was sent: delete the words. Turn typing mode off while others are talking near the microphone. If it happened in a quiet room, report the text you saw. |
| You said something and nothing happened, with nothing shown | The app took it for noise rather than speech, usually because it was very short or quiet. | Say it again a little louder or closer to the microphone. |
| Answers are cut off or not understood with a reasoning model | **Max tokens** is too low for its reasoning. | Raise **Max tokens**. |
| "no microphone was found on this machine", or "the microphone would not open (…)" | No input device, or the app may not use it. | Connect a microphone and check it is the system's input device; on macOS, allow the app under System Settings → Privacy & Security → Microphone. Then press **Voice** again. |
