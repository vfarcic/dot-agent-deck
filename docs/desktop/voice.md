---
title: Voice Control
---

# Voice Control

The desktop app can be driven by voice: open screens, switch daemons, start the New agent flow, and type into an agent. The TUI has no voice control.

Voice is off whenever the app starts. It needs two services, set up in **Settings → Voice**: one that turns speech into text (**Speech**) and one that turns that text into one of the app's commands (**Commands**). Read [What is sent where](#what-is-sent-where) before turning it on.

## Using it

Press **Voice off** at the bottom of the window to turn voice on; the button then reads **Voice on**, and the app listens continuously until you turn it off. Press the button again, or say "voice off", to stop. The microphone is released when voice is off. Your operating system may ask the first time whether the app may use it.

Say "what can I say?" to see every command, split into **On this screen** and **On another screen**. Close the list with **Close**.

What the app did with each thing you said appears beside the button and stays until the next one. When a command can be reversed, an **Undo** button is shown beside it for ten seconds.

To type into an agent, open its [pane](dashboard.md#the-agent-pane) and start with "type", for example "type run the tests". The words after "type" are typed into the agent's prompt, taken from what the Speech service heard rather than from the Commands model's answer, and sent after a short countdown. If the agent in the pane is replaced by a new one before the countdown ends, nothing is sent and the app says so.

## When a command matches several things

If what you said names more than one thing on screen, for example "open the agent" with several agents on the dashboard, the app does not guess. It lists the matches as numbered buttons beside the Voice button, with a **Cancel** button and a countdown, and waits for you to choose:

![The agent dashboard with voice on and "open the agent" heard: beside the Voice button, the sentence saying "agent" matches more than one agent, then the numbered buttons 1. Plan / architecture and 2. Desktop implementation, a Cancel button and a 20 s countdown](/img/voice-choice-desktop.png)

- **Say the number**: "two", "2", "number two", "option two", "the second one" or "the last one", on its own.
- **Say the name** of one of the listed entries, such as "Desktop implementation".
- **Click** an entry, or reach it with Tab and press Enter.

If an entry's name is itself a number or a way of cancelling, for example an agent called "two" or "cancel", saying just that name chooses nothing: the list closes and the app says why. Say the command again, then say "number" and the entry's position, such as "number one", or click the entry.

The command you first gave then runs with the entry you chose; what you said is not sent to the Commands service again. Choosing does not skip a confirmation: if the command stops an agent or closes an orchestration, the confirmation still opens, and you answer it by hand.

To choose nothing, say "cancel", "cancel that", "never mind", "none", "none of them", "neither" or "no" on its own, press **Cancel**, or press Escape while an entry has focus. The list also closes on its own after 20 seconds, and when you turn voice off; nothing runs.

If you say something else while the list is open, the list closes and what you said is treated as a new command. A number that is not on the list, a name that matches more than one entry, or on its own the name of something on screen that is not on the list, closes the list without running anything; say the command again, more specifically.

Nothing runs, and the app says why, if what the list was about changed after it appeared: you moved to another screen, the New agent dialog opened, closed or changed, the directory it showed moved, a daemon's address changed, or the agent, daemon or orchestration you chose is no longer there.

A list is not offered while a confirmation is open, or when more than nine things match; the app then says what matched, and you say the command again, more specifically. Some refusals are never turned into a list, for example "switch to build, not staging", where choosing between the two would let you pick the daemon you just ruled out.

## Typing mode

When you want to dictate a longer prompt, you do not have to start every sentence with "type". Open the agent's pane and say "type on" (or "typing on", "start typing", "dictation on", "start dictation", "keep typing"). From then on, everything you say is typed into that agent's prompt, word for word, until you stop.

While typing mode is on:

![An agent's pane with typing mode on: "Typing to Desktop implementation" on the pane's top edge, and at the bottom of the window the Voice on button, a Stop typing button and the reminder "Typing to Desktop implementation. Say “type off” to stop, “send it” to send."](/img/voice-typing-mode-desktop.png)

- **Typing to** *agent* is shown at the top of the agent's pane and beside the Voice button, with a reminder of what to say to stop and to send, and a **Stop typing** button.
- Nothing is sent to the agent until you say "send it" (or "send", "submit", "enter", "press enter", "go ahead", "finished", "end") on its own, or press Enter yourself. After a send, typing mode stays on, so you can dictate the next prompt the same way.
- Only a handful of things you can say still act as commands, and only when you say them on their own: "type off" (or "typing off", "stop typing", "dictation off", "stop dictation", "done typing"), the send phrases above, and "voice off" (or "mute", "mic off", "stop listening", "stop voice"). Said inside a longer sentence, such as "tell the reviewer to send it when the tests pass", they are typed like anything else. Other commands, such as opening a screen, do not work until you stop typing.
- If you speak for 30 seconds without a pause, what you said is still typed, and the app tells you it reached the limit.

Typing mode ends, and **nothing is sent** when it does, if you:

- say "type off", or say "voice off", which also turns voice off;
- press **Stop typing** (you can also reach it with Tab), which leaves voice on, or press the Voice button, which turns voice off;
- close the pane (for example with Escape), open another screen, open another agent, or switch daemons;
- or a confirmation opens, for example to stop an agent.

It also ends if the agent stops accepting input, for example because it exited, or if it is replaced by a new agent in the same pane. Something you said just before typing mode ended is not typed or sent. Whatever you dictated stays in the agent's prompt, where you can edit it or send it yourself. The app says why typing mode ended. Typing mode never moves to another agent on its own: to dictate to a different agent, open its pane and say "type on" again. If the agent on screen cannot take input, "type on" is refused with the reason.

If the app mishears "type off", the words are typed into the prompt instead of stopping typing mode. They are not sent: press **Stop typing** and delete them.

A bare "type on" or "type off" always switches typing mode, so it cannot be used to type the single word "on" or "off". Say "type the word on" instead.

## Settings → Voice

![Settings → Voice with the default services: Speech on this machine, through a local speech container, and Commands through an OpenAI-compatible API, each with its Endpoint and Model, then Names set to Shared and Max tokens](/img/settings-voice-desktop.png)

| Row | What it sets |
| --- | --- |
| **Speech** | Where speech is turned into text. **On this machine — speech container, no key** (the default) uses a local container; the panel gives the `docker run` command that starts it, and the first thing you say downloads its model. **OpenAI — needs an OpenAI API key** sends your audio to OpenAI. |
| **Commands** | Where the text is turned into a command. **OpenAI-compatible API — needs that provider's API key** (the default, pointed at OpenAI) or **Anthropic API — needs an Anthropic API key**. |
| **Endpoint**, **Model** | The URL and model of each service. Choosing a service fills in its usual values; change them to point at another provider or a local server. |
| **Max tokens** | The longest answer the Commands model may give, 64 to 32768 (4096 by default). A model that reasons spends part of this on its reasoning. |
| **Names** | **Shared** or **Withheld** (below). |
| **… key for** *host* | The API key for a service that needs one: paste it and press **Save**, then **Replace** or **Forget** it later. |

Keys are stored in your operating system's keychain, not in the settings file: one key for **Speech** and one for **Commands**. The panel says whether a key is stored.

## What is sent where

- **To the Speech service:** your audio. With the default local container, it stays on this machine.
- **To the Commands service, on every command:** the words it heard, the app's fixed instructions and answer format, the model name and token limit, and the app's list of commands. When the endpoint is not on this machine, the request also carries your Commands API key. Some utterances are decided on this machine and send nothing: one that starts with "type", "write", "say" or "dictate" followed by words to type; one that is, in its entirety, "end", "send", "send it", "submit", "enter" or "press enter" (case, punctuation and a word such as "okay" or "please" before or after it ignored); while the New agent dialog is open, one that is in its entirety a way of closing it, such as "close", "cancel" or "close new agent"; and, while a [numbered list](#when-a-command-matches-several-things) is open, a number, a listed name or a way of cancelling it. Everything else goes to the Commands service, including other ways of saying submit such as "go ahead". While [typing mode](#typing-mode) is on, nothing you say is sent to the Commands service at all: it is typed into the agent or, for the few phrases that still work, handled on this machine. "type on" and "type off" themselves are also decided on this machine.
- **With Names shared**, each command also sends the names on screen: each agent on the selected daemon with its role, CLI name, status and running tool; each daemon's label, which for a remote daemon is its ssh user, host and any non-default port; while the New agent dialog shows a directory, up to 200 directory names from it; the dialog's Mode chips and agent entries; and each orchestration's title and roles. The app adds no filesystem path, id, prompt text or tool argument of its own, but a name is whatever it was set to, and can itself be a path.
- **With Names withheld**, none of those names is sent, so the commands that name an agent, daemon, directory, mode, agent type or orchestration are unavailable. Your words are still sent as heard.

The panel states the same thing beside the **Commands** setting.
