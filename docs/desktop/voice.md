# Voice Control

The desktop app can be driven by voice: open screens and agents, switch daemons, fill in and submit the New agent dialog, stop agents (after you confirm on screen), and type into an agent. The TUI has no voice control.

Voice needs two services, set in **Settings → Voice**: **Speech** turns your audio into text, and **Commands** turns that text into one of the app's commands. Read [What is sent where](#what-is-sent-where) before turning it on.

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
| An agent's pane | Type into the agent's prompt; submit it |

**Typing into an agent.** Open its [pane](dashboard.md#the-agent-pane) and start with "type", "write", "say" or "dictate", for example "type run the tests". The words after the opener are typed into the agent's prompt, taken from what the Speech service heard rather than from the Commands model's answer. Other phrasings ("tell it to …") work too; the Commands model finds where your words start, and the app checks that they really are your words before typing them. After typing, a five-second countdown runs on screen and then the prompt is sent. Speaking again stops the countdown; another "type …" adds its words to the prompt and starts the countdown again, and otherwise the prompt waits for you to send it. To send, say "send", "send it", "submit", "enter", "press enter" or "end" as the whole utterance, or press `Enter` yourself.

**While voice is on, the app keeps the computer from going to sleep from inactivity**, because speaking produces no keyboard or mouse input. The display can still turn off. On Linux this goes through systemd-logind; a desktop environment whose power manager ignores logind may still suspend.

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

- **To the Speech service:** your audio and the model name. With the default local container, it stays on this machine. The OpenAI speech option also sends your Speech key.
- **To the Commands service, for each utterance it decides:** the words it heard, the app's fixed instructions and answer format, the model name and token limit, and the app's list of commands (each command's id, description, parameter names and kinds, whether it can run on the current screen, and the hint shown when it cannot). When the endpoint is not on this machine, the request also carries your Commands API key.
- **Decided on this machine, sending nothing:** an utterance that starts with the word "type", "write", "say" or "dictate" followed by words to type, and one that is, in its entirety, "end", "send", "send it", "submit", "enter" or "press enter" (case and punctuation ignored). Everything else goes to the Commands service, including other ways of saying submit such as "go ahead". Silence sends nothing.
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
| Answers are cut off or not understood with a reasoning model | **Max tokens** is too low for its reasoning. | Raise **Max tokens**. |
| "no microphone was found on this machine", or "the microphone would not open (…)" | No input device, or the app may not use it. | Connect a microphone and check it is the system's input device; on macOS, allow the app under System Settings → Privacy & Security → Microphone. Then press **Voice** again. |
