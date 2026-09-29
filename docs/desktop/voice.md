# Voice Control

The desktop app can be driven by voice: open screens, switch daemons, start the New agent flow, and type into an agent. The TUI has no voice control.

Voice is off whenever the app starts. It needs two services, set up in **Settings → Voice**: one that turns speech into text (**Speech**) and one that turns that text into one of the app's commands (**Commands**). Read [What is sent where](#what-is-sent-where) before turning it on.

## Using it

Press **Voice off** at the bottom of the window to turn voice on; the button then reads **Voice on**, and the app listens continuously until you turn it off. Press the button again, or say "voice off", to stop. The microphone is released when voice is off. Your operating system may ask the first time whether the app may use it.

Say "what can I say?" to see every command, split into **On this screen** and **On another screen**. Close the list with **Close**.

What the app did with each thing you said appears beside the button and stays until the next one. When a command can be reversed, an **Undo** button is shown beside it for ten seconds.

To type into an agent, open its [pane](dashboard.md#the-agent-pane) and start with "type", for example "type run the tests". The words after "type" are typed into the agent's prompt, taken from what the Speech service heard rather than from the Commands model's answer, and sent after a short countdown.

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
- **To the Commands service, on every command:** the words it heard, the app's fixed instructions and answer format, the model name and token limit, and the app's list of commands. When the endpoint is not on this machine, the request also carries your Commands API key. Two kinds of utterance are decided on this machine and send nothing: one that starts with "type", "write", "say" or "dictate" followed by words to type, and one that is, in its entirety, "end", "send", "send it", "submit", "enter" or "press enter" (case and punctuation ignored). Everything else goes to the Commands service, including other ways of saying submit such as "go ahead".
- **With Names shared**, each command also sends the names on screen: each agent on the selected daemon with its role, CLI name, status and running tool; each daemon's label, which for a remote daemon is its ssh user, host and any non-default port; while the New agent dialog shows a directory, up to 200 directory names from it; the dialog's Mode chips and agent entries; and each orchestration's title and roles. The app adds no filesystem path, id, prompt text or tool argument of its own, but a name is whatever it was set to, and can itself be a path.
- **With Names withheld**, none of those names is sent, so the commands that name an agent, daemon, directory, mode, agent type or orchestration are unavailable. Your words are still sent as heard.

The panel states the same thing beside the **Commands** setting.
