# Voice Control

The desktop app can be driven by voice: open screens and agents, switch daemons, fill in and submit the New agent dialog, stop agents (after you confirm on screen), and type or dictate into an agent. The TUI has no voice control.

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

What the app did with each thing you said appears beside the Voice button and stays until the next one. When a command can be reversed, an **Undo** button is shown beside it for ten seconds.

What you can do by voice, by screen:

| Where | You can |
| --- | --- |
| Anywhere, including an agent's pane | Close what is open over the screen (a pane, Settings, a dialog); list the commands; turn voice off |
| Dashboard | Open Settings; switch which daemon the app shows; open an agent's pane; open New agent; stop an agent or close an orchestration (the app shows the same confirmation as the buttons, and nothing stops until you confirm by hand) |
| New agent dialog | Choose the daemon; open a directory, go up, use the directory shown; choose a Mode chip or the agent to run; set the Name; start; discard |
| An agent's pane | Type into the agent's prompt; submit it; turn [typing mode](#typing-mode) on and off |

When what you said matches more than one thing on screen, the app lists the matches for you to choose from; see [When a command matches several things](#when-a-command-matches-several-things).

A command that names an agent, such as "stop the planner" or "close the review orchestration", runs nothing, and the app says why, if you switch daemons or that agent is replaced by a new one under the same name while the app is still working out what you said (which the app can tell only when the daemon reports when each agent started). Say it again to act on the agent that is there now.

**Typing into an agent.** Open its [pane](dashboard.md#the-agent-pane) and start with "type", "write", "say" or "dictate", for example "type run the tests". The words after the opener are typed into the agent's prompt, taken from what the Speech service heard rather than from the Commands model's answer. Other phrasings ("tell it to …") work too; the Commands model finds where your words start, and the app checks that they really are your words before typing them. After typing, a five-second countdown runs on screen and then the prompt is sent. Speaking again stops the countdown; another "type …" adds its words to the prompt and starts the countdown again, and otherwise the prompt waits for you to send it. To send, say "send", "send it", "submit", "enter", "press enter" or "end" as the whole utterance, or press `Enter` yourself. To dictate a longer prompt without starting every sentence with "type", use [typing mode](#typing-mode).

Nothing is typed, and the app says why, if a confirmation is open, or if the pane on screen is no longer the one you were looking at when you spoke — for example because you opened another agent while the app was still working out what you said. Once the words are typed, the send is called off, and the app says so, if before it happens you close the pane, open another screen or agent, switch daemons, or a confirmation opens: the words stay in the agent's prompt, unsent. It is also called off if the agent in the pane is replaced by a new one. Telling a replacement apart needs a daemon that reports when each agent started; with one that does not, the app cannot tell a replacement from the original agent.

**While voice is on, the app keeps the computer from going to sleep from inactivity**, because speaking produces no keyboard or mouse input. The display can still turn off. On Linux this goes through systemd-logind; a desktop environment whose power manager ignores logind may still suspend.

## When a command matches several things

If what you said names more than one thing on screen, for example "open the agent" with several agents on the dashboard, the app does not guess. It lists the matches as numbered buttons beside the Voice button, with a **Cancel** button and a countdown, and waits for you to choose:

![The agent dashboard with voice on and "open the agent" heard: beside the Voice button, the sentence saying "agent" matches more than one agent, then the numbered buttons 1. Plan / architecture and 2. Desktop implementation, a Cancel button and a 20 s countdown](/img/voice-choice-desktop.png)

- **Say the number**: "two", "2", "number two", "option two", "the second one" or "the last one", on its own.
- **Say the name** of one of the listed entries on its own, such as "Desktop implementation". A sentence that only contains a listed name, such as "stop Planner", is not an answer: the list closes and that sentence runs as a new command.
- **Click** an entry, or reach it with `Tab` and press `Enter`.

If an entry's name is itself a number or a way of cancelling, for example an agent called "two" or "cancel", saying just that name chooses nothing: the list closes and the app says why. Say the command again, then say "number" and the entry's position, such as "number one", or click the entry.

The command you first gave then runs with the entry you chose; what you said is not sent to the Commands service again. Choosing does not skip a confirmation: if the command stops an agent or closes an orchestration, the confirmation still opens, and you answer it by hand.

To choose nothing, say "cancel", "cancel that", "never mind", "none", "none of them", "neither" or "no" on its own, press **Cancel**, or press `Escape` while an entry has focus. The list also closes on its own after 20 seconds, and when you turn voice off; nothing runs.

If you say something else while the list is open, the list closes and what you said is treated as a new command. A number that is not on the list, a name that matches more than one entry, or on its own the name of something on screen that is not on the list, closes the list without running anything; say the command again, more specifically.

Nothing runs, and the app says why, if what the list was about changed after it appeared: you moved to another screen, the New agent dialog opened, closed or changed, the directory it showed moved, a daemon's address changed, or the agent, daemon or orchestration you chose is no longer there. The same goes if you switch daemons, or if the agent you chose was replaced by a new one under the same name after you gave the command (which the app can tell only when the daemon reports when each agent started).

A list is not offered while a confirmation is open, or when more than nine things match; the app then says what matched, and you say the command again, more specifically. Some refusals are never turned into a list, for example "switch to build, not staging", where choosing between the two would let you pick the daemon you just ruled out.

## Typing mode

When you want to dictate a longer prompt, you do not have to start every sentence with "type". Open the agent's pane and say "type on" (or "typing on", "start typing", "dictation on", "start dictation", "keep typing"). From then on, everything you say is typed into that agent's prompt, word for word, until you stop.

While typing mode is on:

![An agent's pane with typing mode on: "Typing to Desktop implementation" on the pane's top edge, and at the bottom of the window the Voice on button, a Stop typing button and the reminder "Typing to Desktop implementation. Say “type off” to stop, “send it” to send."](/img/voice-typing-mode-desktop.png)

- **Typing to** *agent* is shown at the top of the agent's pane and beside the Voice button, with a reminder of what to say to stop and to send, and a **Stop typing** button.
- Nothing is sent to the agent until you say "send it" (or "send", "submit", "enter", "press enter", "go ahead", "finished", "end") on its own, or press `Enter` yourself. After a send, typing mode stays on, so you can dictate the next prompt the same way.
- Only a handful of things you can say still act as commands, and only when you say them on their own: "type off" (or "typing off", "stop typing", "dictation off", "stop dictation", "done typing"), the send phrases above, and "voice off" (or "mute", "mic off", "stop listening", "stop voice"). Said inside a longer sentence, such as "tell the reviewer to send it when the tests pass", they are typed like anything else. Other commands, such as opening a screen, do not work until you stop typing.
- If you speak for 30 seconds without a pause, what you said is still typed, and the app tells you it reached the limit.

Typing mode ends, and **nothing is sent** when it does, if you:

- say "type off", or say "voice off", which also turns voice off;
- press **Stop typing** (you can also reach it with `Tab`), which leaves voice on, or press the Voice button, which turns voice off;
- close the pane (for example with `Escape`), open another screen, open another agent, or switch daemons;
- or a confirmation opens, for example to stop an agent.

It also ends if the agent stops accepting input, for example because it exited, or if it is replaced by a new agent in the same pane (which the app can tell only when the daemon reports when each agent started). Something you said just before typing mode ended that the app was still working out is not typed or sent, but words it had already handed to the agent can still appear in the prompt after typing mode ends; they are not sent. If the agent you were typing to is still there, whatever you dictated stays in its prompt, where you can edit it or send it yourself; a replacement agent starts with an empty prompt. The app says why typing mode ended. Typing mode never moves to another agent on its own: to dictate to a different agent, open its pane and say "type on" again. If the agent on screen cannot take input, "type on" is refused with the reason.

If the app mishears "type off", the words are typed into the prompt instead of stopping typing mode. They are not sent: press **Stop typing** and delete them.

A bare "type on" or "type off" always switches typing mode, so it cannot be used to type the single word "on" or "off". Say "type the word on" instead.

## Settings → Voice

![Settings → Voice with the default services: Speech on this machine, through a local speech container, and Commands through an OpenAI-compatible API, each with its Endpoint and Model, then Names set to Shared and Max tokens](/img/settings-voice-desktop.png)

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
- **Decided on this machine, sending nothing:** an utterance that starts with the word "type", "write", "say" or "dictate" followed by words to type; one that is, in its entirety, "end", "send", "send it", "submit", "enter" or "press enter" (case, punctuation and a word such as "okay" or "please" before or after it ignored); while the New agent dialog is open, one that is in its entirety a way of closing it, such as "close", "cancel" or "close new agent"; while a [numbered list](#when-a-command-matches-several-things) is open, a number, a listed name or a way of cancelling it; and "type on" and "type off" themselves. While [typing mode](#typing-mode) is on, nothing you say is sent to the Commands service at all: it is typed into the agent or, for the few phrases that still work, handled on this machine. Everything else goes to the Commands service, including other ways of saying submit such as "go ahead". Silence sends nothing.
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
| "type off" was typed into the prompt instead of stopping typing mode | The app misheard it. | Nothing was sent: press **Stop typing** and delete the words. |
| Answers are cut off or not understood with a reasoning model | **Max tokens** is too low for its reasoning. | Raise **Max tokens**. |
| "no microphone was found on this machine", or "the microphone would not open (…)" | No input device, or the app may not use it. | Connect a microphone and check it is the system's input device; on macOS, allow the app under System Settings → Privacy & Security → Microphone. Then press **Voice** again. |
