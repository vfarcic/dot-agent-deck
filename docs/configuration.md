# Configuration

This page is the reference for the files and environment variables that configure dot-agent-deck, and the tasks you do with them. The TUI and the desktop app are both clients of one daemon: settings that belong to the daemon apply to both, while each client also has a file of its own. `dot-agent-deck docs configuration` prints this page for the version you have installed.

## Where configuration lives

| File | What it holds | Read by | Default path | Override |
| --- | --- | --- | --- | --- |
| `config.toml` | `default_command`, `default_dir`, the TUI's bell ([reference](#configtoml-reference)) | the TUI, `dot-agent-deck config`, and the daemon (for the desktop app's New agent and for dispatch) | `~/.config/dot-agent-deck/config.toml` | `DOT_AGENT_DECK_CONFIG` |
| `.dot-agent-deck.toml` | a project's orchestrations, `worker_response_timeout_minutes`, `[features]` ([reference](#dot-agent-decktoml-reference)) | the TUI and the daemon; the desktop app through the daemon | the project directory | none (`DOT_AGENT_DECK_FEATURES_CONFIG` for `[features]` only) |
| `keybindings.toml` | TUI key remapping, see [Keyboard Shortcuts](keyboard-shortcuts.md) | the TUI | `~/.config/dot-agent-deck/keybindings.toml` | `DOT_AGENT_DECK_KEYBINDINGS` |
| `schedules.toml` | scheduled tasks, see [Schedules](scheduled-tasks.md) | the daemon and `dot-agent-deck schedule` | `$XDG_CONFIG_HOME/dot-agent-deck/schedules.toml`, else `~/.config/dot-agent-deck/schedules.toml` | `DOT_AGENT_DECK_SCHEDULES` |
| `remotes.toml` | registered remote hosts, see [Remote Environments](remote-environments.md) | `dot-agent-deck remote`/`connect` and the desktop app | `~/.config/dot-agent-deck/remotes.toml` | `DOT_AGENT_DECK_REMOTES` |
| `desktop.toml` | the desktop app's own settings ([reference](#desktoptoml-reference)) | the desktop app | `~/.config/dot-agent-deck/desktop.toml` | `DOT_AGENT_DECK_DESKTOP_CONFIG` |
| `session.toml` | the TUI's saved session (panes to restore, and its copy of the last command used) | the TUI | `~/.config/dot-agent-deck/session.toml` | `DOT_AGENT_DECK_SESSION` |
| `config-gen-state.json`, `star-prompt-state.json` | small TUI state files | the TUI | beside `config.toml` | `DOT_AGENT_DECK_CONFIG_GEN_STATE`, `DOT_AGENT_DECK_STAR_PROMPT` |

`~/.config/dot-agent-deck/` is used on macOS and Linux whatever `XDG_CONFIG_HOME` says; of the files above, only `schedules.toml` consults `XDG_CONFIG_HOME`. On Windows the directory is `%APPDATA%\dot-agent-deck`. Every override variable takes the full path of the file, not a directory.

A daemon on a remote host reads the files on *that* host. When you `connect` to a remote, the TUI you see also runs on the remote host, so it reads the remote's `config.toml` and `keybindings.toml`, not yours.

## Set the command new agents start with

`default_command` is the command the new-agent form pre-fills. Set it on the machine the daemon runs on:

```bash
dot-agent-deck config set default_command "claude"
dot-agent-deck config get default_command     # prints: claude
```

What it affects:

- **TUI, `Ctrl+n` New Agent form:** the **Command** field is pre-filled with `default_command`. When `default_command` is empty, it is pre-filled with the deck's last command (below); with neither, it starts blank. A blank command starts your shell. The field is only pre-filled, never run until you submit.
- **TUI, the schedule, `schedule: issues` and dispatcher modes:** a blank **Command** becomes `default_command`, or `claude` when that is empty, because these modes need an agent rather than a shell. The Schedules manager's **Add** and **Edit** pre-fill **Command** the same way.
- **Desktop app, New agent:** the **Command** field is pre-filled with the chosen daemon's `default_command`, else that deck's last command (below), else blank. A blank command starts the daemon's default shell for a plain agent; for the schedule and dispatcher modes a blank command becomes `default_command`, or `claude` when that is empty.
- **Daemon:** a scheduled issue-dispatch task with no `command` starts `default_command`, or Claude Code when it is empty. A single-agent `dot-agent-deck dispatch` starts the same command as the agent that dispatched it, and `default_command` (or Claude Code) only when that agent was started with no command or with one that only opens a shell. The daemon reads the file each time, so no restart is needed.

The **last command** is the most recent command started from a New Agent form on that deck, from the TUI or the desktop app. The deck remembers it on its own host, so it survives restarting the TUI, the app or the daemon, and both clients offer the same one. Each deck remembers its own last command. When the TUI attaches to a deck that has none yet, it hands that deck the command the TUI last started, from whichever deck that was, so a deck can start out offering a command first used on another. A deck from a release before this one does not remember it: there the TUI offers the last command it started (kept in `session.toml`) and the desktop app the last command it started on that deck since the app was opened.

Check it worked: open the New Agent form (`Ctrl+n` in the TUI, **New agent** in the desktop app) and look at the **Command** field. The TUI reads `config.toml` when it starts, so restart the TUI after changing it; the desktop app picks up the change the next time it opens New agent.

## Set the directory new agents start browsing in

`default_dir` is the directory that creating an agent on this daemon starts browsing in. It must be an absolute path on the daemon's host:

```bash
dot-agent-deck config set default_dir "/home/me/projects"
dot-agent-deck config set default_dir ""      # unset it
```

`config set` refuses a relative path (`Invalid default_dir: "…" is not an absolute path (empty unsets it)`, exit code 1). The TUI's `Ctrl+n` directory picker and the Schedules manager's **Add** open there, and so does the desktop app's New agent dialog. Editing a schedule still opens at that schedule's own directory. It is a starting point, not a limit: you can still go above it with `..`.

If the value is unset, or names something that is missing, not a directory, or cannot be opened, nothing fails: the TUI's picker opens in the directory the TUI was launched from, and the desktop dialog in the home directory on the daemon's host. If the picker does not open where you expect, run `dot-agent-deck config get default_dir` on the daemon's host and check that the directory exists and that you can list it.

## Turn the TUI's terminal bell on or off

The TUI rings the terminal bell when an agent's status changes to one you have enabled. The desktop app does not use these settings.

```bash
dot-agent-deck config set bell.enabled false           # no bell at all
dot-agent-deck config set bell.on_idle true            # also ring when an agent goes idle
```

| Key | Default | Rings when an agent becomes |
| --- | --- | --- |
| `bell.enabled` | `true` | master switch; `false` silences every bell below |
| `bell.on_waiting_for_input` | `true` | waiting for input |
| `bell.on_idle` | `false` | idle |
| `bell.on_error` | `true` | errored, or blocked by a provider usage limit |

Restart the TUI for a change to take effect.

## `config.toml` reference

`dot-agent-deck config get <key>` prints a value; `dot-agent-deck config set <key> <value>` writes one. Both exit `1` with `Unknown config key: <key>` and the list of keys for a key that does not exist. Booleans take exactly `true` or `false`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default_command` | string | `""` | [Command new agents start with](#set-the-command-new-agents-start-with) |
| `default_dir` | absolute path, or `""` | `""` (unset) | [Directory agent creation starts browsing in](#set-the-directory-new-agents-start-browsing-in) |
| `auto_config_prompt` | boolean | `true` | Accepted and stored, but it currently changes nothing; see [Generate a config with your agent](#generate-a-config-with-your-agent-tui) |
| `bell.enabled` | boolean | `true` | [TUI bell](#turn-the-tuis-terminal-bell-on-or-off) master switch |
| `bell.on_waiting_for_input` | boolean | `true` | Bell on waiting for input |
| `bell.on_idle` | boolean | `false` | Bell on idle |
| `bell.on_error` | boolean | `true` | Bell on error or blocked |

The file is TOML, and you can edit it by hand:

```toml
default_command = "claude"
default_dir = "/home/me/projects"
auto_config_prompt = true

[bell]
enabled = true
on_waiting_for_input = true
on_idle = false
on_error = true
```

`config set` rewrites the whole file from the keys above, so comments and any other keys are removed. If the file does not parse, the TUI and `config` print `Invalid config at <path>: <error>` and use the defaults, and a `config set` then replaces the unparseable file with the defaults plus the key you set. Fix a parse error by hand before running `config set`.

## Create a project configuration

A project's `.dot-agent-deck.toml` sits in the directory you start agents in. It is where orchestrations are defined; [Orchestration](orchestration.md) explains them and has the full role reference.

### Start from the template

```bash
cd your-project
dot-agent-deck init              # or: dot-agent-deck init --path <dir>
```

`init` writes a commented two-role orchestration (`orchestrator` and `worker`, both running `claude`) and prints `Created ./.dot-agent-deck.toml`. In a git repository it also adds `.dot-agent-deck/` (the deck's per-project working directory) to `.git/info/exclude` when it is not listed there yet, and prints `Excluded .dot-agent-deck/ in .git/info/exclude`. It refuses to overwrite an existing file: it prints `<path> already exists` and exits `1`.

### Check it

```bash
dot-agent-deck validate          # or: dot-agent-deck validate --path <dir>
```

| Output | Exit code | Meaning |
| --- | --- | --- |
| `Config is valid.` | `0` | no findings |
| lines of `[warning] '<scope>': <message>` only | `0` | usable; read the warnings |
| any `[error] '<scope>': <message>` line | `1` | the file has a problem the deck will not work around |
| `No .dot-agent-deck.toml found in <dir>` | `1` | wrong directory |
| `Failed to parse <path>: …` | `1` | TOML syntax error, or an `extends` that cannot be resolved |

Errors include: an orchestration with fewer than two roles, not exactly one role with `start = true`, an empty, duplicate or path-like (`/`, `\`, `..`) role name, an empty `command`, and more than one orchestration with `default = true`. Warnings include: a duplicate orchestration name, a worker role with no `description`, an unknown `agent` name, a role whose command does not reveal its agent, several orchestrations with none marked `default`, and a leftover `[[modes]]` block. [Validate your config](orchestration.md#validate-your-config) covers the orchestration checks.

`validate` does not report a key it does not recognise, or a top-level key placed below a table header (see [Top-level keys](#top-level-keys)).

### Generate a config with your agent (TUI)

This is a TUI feature; the desktop app has no equivalent. It asks an agent that is already running in the project to write the file for you.

1. Start an agent in the project and select its card on the dashboard.
2. Press `g` (or click **Generate**). A dialog titled **Generate .dot-agent-deck.toml** opens.
3. Choose with the arrow keys and `Enter`: **Yes** sends your agent a prompt asking it to analyse the project, propose an orchestration and write it after your approval ([Quick setup](orchestration.md#quick-setup) describes what the agent does); **No** or `Esc` closes the dialog; **Never** closes it and records the directory in `config-gen-state.json`.
4. When the agent has written the file, run `dot-agent-deck validate` in the project, then press `Ctrl+n`, pick the directory, and choose the orchestration on the **Mode** row.

The dialog opens only when you press `g` or click **Generate**. The dialog's hint `Disable: dot-agent-deck config set auto_config_prompt false` and the **Never** choice currently have no further effect, because nothing opens the dialog on its own. If `g` shows `No active agent to send prompt to.`, the selected card has no running agent pane.

## `.dot-agent-deck.toml` reference

### Where it is read from

- **Orchestrations** are read from `.dot-agent-deck.toml` in the directory you picked, not from a parent directory. The TUI reads it when you open the New Agent form and when it opens an orchestration tab; the daemon reads it again on each delegation, dispatch and scheduled run, so an edit applies to the next one without restarting anything.
- **The desktop app** asks the daemon for a directory's orchestrations. That read refuses a `.dot-agent-deck.toml` that is a symlink, is not a regular file, or is larger than 1 MiB, and the dialog then offers no orchestrations for that directory. The TUI follows a symlink.
- **`[features]`** is read differently; see [Turn on experimental features](#turn-on-experimental-features).

### Top-level keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `worker_response_timeout_minutes` | integer | `120` | How long a delegated worker may go without `work-done` before the daemon reports it to its orchestrator. `1`–`10080` (seven days) is used as written. `0` turns the report off; it does not mean "immediately". A larger value is ignored, with a warning in the log, and `120` is used. See [Idle Workers & Notifications](idle-workers-and-notifications.md). |
| `[[orchestrations]]` | array of tables | none | Orchestration definitions, including `name`, `default`, `extends` and `[[orchestrations.roles]]` (`name`, `command`, `agent`, `start`, `description`, `prompt_template`, `clear`). The full reference is in [Orchestration](orchestration.md#configuration-reference). |
| `[features]` | table | | `experimental = true` or `false` (default `false`). See [Turn on experimental features](#turn-on-experimental-features). |
| `[[modes]]` | | | Workspace modes were removed. A leftover block is ignored; `validate` warns `workspace modes were removed (#1199); this block is ignored and can be deleted`. |

Unknown keys are ignored without a warning, so a misspelled key silently keeps its default.

**A top-level key must come before the first table header in the file.** TOML assigns every key after a header such as `[[orchestrations]]`, `[[orchestrations.roles]]` or `[features]` to that table, so a `worker_response_timeout_minutes` appended at the end of the file belongs to the last table, where nothing reads it. The file still parses, `validate` still prints `Config is valid.`, and the default stays in effect:

```toml
worker_response_timeout_minutes = 30   # top of the file: applies

[[orchestrations]]
name = "team"
# ... roles ...
```

Scheduled tasks are not in this file; they live in the global `schedules.toml` ([Schedules](scheduled-tasks.md)).

## Turn on experimental features

Some new surfaces ship behind one switch, `experimental`, which is off by default. In the TUI it enables, among others, the `Ctrl+e` command-entry lock in orchestration tabs and the `schedule: issues` mode; in the desktop app, the deck screen and the Projects, Prompts, Orchestrations and Agent Profiles screens.

For the TUI and the daemon, either set it in the project's `.dot-agent-deck.toml`:

```toml
[features]
experimental = true
```

or set `DOT_AGENT_DECK_EXPERIMENTAL=1` in the environment, which wins over the file. `1` or `true` (any case) turns it on; any other value, including an empty one, turns it off.

- The TUI and the daemon each look for the file starting in the directory they were launched from and walking up to the first `.dot-agent-deck.toml` that is a regular file owned by you. They re-read it about every two seconds, so an edit applies without a restart. `DOT_AGENT_DECK_FEATURES_CONFIG=<file>` names the file outright.
- The daemon's value is fixed by where and how it was started. A daemon started from another directory, or before you set the variable, keeps its own value; stop it with `dot-agent-deck daemon stop` and relaunch.
- The desktop app does not look for a project file. It reads `DOT_AGENT_DECK_EXPERIMENTAL`, or else the file named by `DOT_AGENT_DECK_FEATURES_CONFIG`, from its own environment once at startup, so set one of them before launching the app.

Check it worked: with `DOT_AGENT_DECK_LOG` set (see [Environment variables](#environment-variables)), the log contains `experimental flag: ON` with the file it came from. In the TUI, `Ctrl+e` in an orchestration tab responds only when the flag is on.

## Environment variables

**Variables are read by the process that has them.** The daemon is usually started for you by the first client that needs one, and it inherits that client's environment and working directory. After that, setting a variable in a new shell does not reach the running daemon. To apply a variable the daemon reads, stop it and start a client from an environment that has the variable:

```bash
dot-agent-deck daemon stop                    # refuses while agents are running; see below
DOT_AGENT_DECK_LOG=1 dot-agent-deck           # the new TUI starts a new daemon with it
```

`daemon stop` refuses while the daemon manages running agents; `--force` stops it anyway and ends those agents. `dot-agent-deck daemon status` lists what it manages. For the desktop app, set the variable in the environment you launch the app from.

### Paths and endpoints

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `DOT_AGENT_DECK_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck.sock`; without `XDG_RUNTIME_DIR`, `<temp dir>/dot-agent-deck-<uid>/hook.sock` | every process | Socket agents' hooks and `work-done`/`delegate` send events to. The daemon passes its socket path to the agents it starts. |
| `DOT_AGENT_DECK_ATTACH_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock`; without `XDG_RUNTIME_DIR`, `<temp dir>/dot-agent-deck-<uid>/attach.sock` | every process | Socket the TUI and the desktop app connect to. |
| `DOT_AGENT_DECK_STATE_DIR` | `$XDG_STATE_HOME/dot-agent-deck`, else `~/.local/state/dot-agent-deck` (`%LOCALAPPDATA%\dot-agent-deck` on Windows) | the client that starts a daemon | Where a daemon started in the background writes `daemon.log` (its output) and `spawn.lock`. An empty value counts as unset. |
| `DOT_AGENT_DECK_CONFIG`, `DOT_AGENT_DECK_SESSION`, `DOT_AGENT_DECK_KEYBINDINGS`, `DOT_AGENT_DECK_SCHEDULES`, `DOT_AGENT_DECK_REMOTES`, `DOT_AGENT_DECK_DESKTOP_CONFIG`, `DOT_AGENT_DECK_CONFIG_GEN_STATE`, `DOT_AGENT_DECK_STAR_PROMPT` | see [Where configuration lives](#where-configuration-lives) | the process reading that file | Full path of that file. |

`<temp dir>` is `$TMPDIR` when set, else `/tmp`; on macOS `$TMPDIR` is normally a per-user directory under `/var/folders`. The deck creates `dot-agent-deck-<uid>` with mode `0700`. If another user already owns a directory of that name, the daemon uses a sibling named `dot-agent-deck-<uid>.` followed by 16 random hex digits instead, and clients find it without configuration. A daemon also listens on `/tmp/dot-agent-deck-<uid>.sock` and `/tmp/dot-agent-deck-attach-<uid>.sock` when it can, so clients from older releases find it. On Windows the endpoints are the named pipes `\\.\pipe\dot-agent-deck-<user>-hook` and `\\.\pipe\dot-agent-deck-<user>-attach`.

To see where the running daemon is listening, run `dot-agent-deck daemon endpoint`; it prints the attach endpoint only when a live daemon answered there. When you set either socket variable, set it to the same value for every process that should reach the same daemon, and pick a directory only you can write to.

### Logging

| Variable | Default | Meaning |
| --- | --- | --- |
| `DOT_AGENT_DECK_LOG` | unset (no log file) | Turns on the log file for the process that has it, including a daemon it starts. `1` or an empty value writes to `/tmp/dot-agent-deck.log` on macOS and Linux and to `dot-agent-deck.log` in the temp directory on Windows; any other value is the file path. The file is appended to. The daemon reads it only when it starts, so a daemon that is already running has to be restarted to pick it up; see [Troubleshooting › Enabling Debug Logs](troubleshooting.md#enabling-debug-logs), which also covers the desktop app. |
| `RUST_LOG` | `error,dot_agent_deck=info` | Verbosity of that log, in [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), added after the default: `RUST_LOG=dot_agent_deck=debug` raises the deck to debug. It does nothing unless `DOT_AGENT_DECK_LOG` is also set. |

A daemon started in the background also writes its standard output and error to `daemon.log` in the state directory above, whether or not `DOT_AGENT_DECK_LOG` is set. [Troubleshooting](troubleshooting.md#enabling-debug-logs) explains how to capture a useful log.

### Features

| Variable | Default | Meaning |
| --- | --- | --- |
| `DOT_AGENT_DECK_EXPERIMENTAL` | unset | `1`/`true` turns the experimental flag on, any other value turns it off; wins over `[features]`. See [Turn on experimental features](#turn-on-experimental-features). |
| `DOT_AGENT_DECK_FEATURES_CONFIG` | unset | Path of the `.dot-agent-deck.toml` whose `[features]` table sets the flag. |

### Daemon behaviour

Set these in the daemon's environment (see the start of this section). Millisecond values must be whole non-negative numbers; a value that is not is ignored with a warning in the log, and a value above the maximum is lowered to it.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` | `30` | The daemon exits this many seconds after it has no attached client, no agents and no pending schedules. `0` keeps it running. A value that is not a whole number uses the default. |
| `DOT_AGENT_DECK_HOOK_PROVENANCE` | enforce | `warn` (any case) makes the daemon accept, with a warning, a `work-done`/`delegate`/`dispatch` or a status update from an agent pane that carries no hook capability token, which happens when the `dot-agent-deck` on the agent's `PATH` is older than the daemon. Any other value keeps refusing. See [Troubleshooting](troubleshooting.md). |
| `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` | `1000`, or `5000`/`8000` for some agents | Extra wait between a worker looking ready and its task being typed in. `0` removes the wait; maximum `30000`. See [Orchestration](orchestration.md). |
| `DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS` | `20000,40000,80000` | When a delegated task shows no sign of being received, the waits before re-sending it, comma-separated. Each entry is kept within `100`–`300000`; at most 8 entries are read. `0` or an empty value turns re-sending off; any unparseable entry makes the whole value fall back to the default. |
| `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` | `30000`, or `worker_response_timeout_minutes` if shorter; none when that is `0` | How long a worker that received a task may report nothing at all before its orchestrator is told. `0` turns the report off, and a non-zero value turns it on even when `worker_response_timeout_minutes = 0`; maximum `30000`. See [Idle Workers & Notifications](idle-workers-and-notifications.md). |
| `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` | `30000` | How long a delegated worker must stay waiting for input before its orchestrator is told. `0` turns the notice off; maximum `600000`. |
| `DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` | `60000` | How long an automatic prompt waits for you to finish text you are typing in that pane before it is written anyway. `0` writes immediately; maximum `600000`. Read once when the daemon starts. |
| `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` | `5000` | For a schedule that reuses its tab, how long the pane must be free of your typing before the prompt is delivered. See [Schedules](scheduled-tasks.md). |
| `DOT_AGENT_DECK_COORDINATION_RETENTION_DAYS` | `14` | Each time the deck writes an orchestration's context into the project's `.dot-agent-deck/` directory, Markdown files there last modified more than this many days ago are deleted, except `orchestrator-context.md`. Subdirectories and symlinks are left alone. `0` turns this off; a value that is not a whole number uses the default. Set it where both the TUI and the daemon are started, since either can start an orchestration. |

### Remote and desktop

| Variable | Default | Meaning |
| --- | --- | --- |
| `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` | `10` | Seconds `connect` and the remote checks allow an ssh probe, kept within `1`–`3600`. Raise it for slow links; see [Remote Recipes](remote-recipes.md). |
| `DOT_AGENT_DECK_BINARY` | unset | The `dot-agent-deck` executable the desktop app starts a local daemon with. Unset, the app looks next to its own executable, then in its parent directories, then on `PATH`. If it is set to something that is not an executable file, the app reports `DOT_AGENT_DECK_BINARY is not an executable file: <path>`. |

### Set by the deck

The deck puts these in each agent's environment; you do not set them. `DOT_AGENT_DECK_PANE_ID` names the pane and `DOT_AGENT_DECK_PANE_CAPABILITY` carries the token that lets that pane's `work-done`, `delegate`, `dispatch` and status updates be accepted. Running those commands in a shell the deck did not start (a plain terminal, or a tool that strips the environment) fails because these are missing; see [Orchestration](orchestration.md) and [Troubleshooting](troubleshooting.md).

## `desktop.toml` reference

The desktop app keeps its settings in `desktop.toml`, beside the TUI's files: `~/.config/dot-agent-deck/desktop.toml` on macOS and Linux, or the path in `DOT_AGENT_DECK_DESKTOP_CONFIG`. The app writes it as you change **Settings**, and you can also edit it by hand while the app is closed. The TUI's `config.toml` and `keybindings.toml` do not configure the app, except that the app gets `default_command` and `default_dir` from the daemon, which reads them from `config.toml` on its host. [Desktop app → Settings](desktop/settings.md) describes the screen.

A new file looks like this; `[endpoints]` and `[voice]` appear once you change those settings:

```toml
version = 1

[appearance]
mode = "system"

[zoom]
level = 1.0
```

| Key | Type | Default | Allowed values |
| --- | --- | --- | --- |
| `version` | integer | `1` | written by the app |
| `appearance.mode` | string | `"system"` | `"system"` (follow the OS), `"light"`, `"dark"`; an unrecognised value means `"system"` |
| `zoom.level` | number | `1.0` | `0.75`, `0.9`, `1.0`, `1.1`, `1.25`, `1.5`, `1.75`, `2.0`, `2.5`, `3.0`; another number is snapped to the nearest |
| `endpoints.selection` | string | `"local"` | `"local"`, `"all"` (every daemon at once), or the `id` of a remote deck in `remotes.toml` |
| `voice.activation` | string | `"toggle"` | `"toggle"` |
| `voice.labels` | string | `"shared"` | `"shared"`, `"withheld"` |
| `voice.transcription.backend` | string | `"local"` | `"local"` (no key; the endpoint must be on this machine), `"remote"` |
| `voice.transcription.endpoint` | URL | `http://127.0.0.1:18000/v1/audio/transcriptions` for `local`, `https://api.openai.com/v1/audio/transcriptions` for `remote` | any URL; a `local` backend refuses one that is not a loopback address |
| `voice.transcription.model` | string | `Systran/faster-whisper-tiny.en` for `local`, `whisper-1` for `remote` | |
| `voice.intent.backend` | string | `"openai_compatible"` | `"openai_compatible"`, `"anthropic"` |
| `voice.intent.endpoint` | URL | `https://api.openai.com/v1/chat/completions` for `openai_compatible`, `https://api.anthropic.com/v1/messages` for `anthropic` | |
| `voice.intent.model` | string | `gpt-5-mini` for `openai_compatible`, `claude-haiku-4-5` for `anthropic` | |
| `voice.intent.max_tokens` | integer | `4096` | `64`–`32768` |

The `voice` keys are absent until you change a voice setting; [Voice Control](desktop/voice.md) explains them. An omitted endpoint or model takes the value for the backend you chose. The app does not write API keys to this file; it keeps them in the operating system's keychain. The list of remote daemons is not in this file either: it is `remotes.toml`, shared with the CLI, and the app moves any `[[endpoints.remote]]` rows written by an older version there the first time it loads the file.

A missing file means the defaults. When the file cannot be read or parsed, the app starts with the defaults and the **Settings** footer says what is wrong instead of showing the path. While the file on disk cannot be read, the app refuses to save over it, so fix or remove it to save settings again. A save writes only the values that changed, so your comments and any keys the app does not know are kept.

## Run a command repeatedly: `dot-agent-deck watch`

A standalone helper, similar to Linux `watch`, for a pane that should show a command's output refreshed:

```bash
dot-agent-deck watch --interval 2 "kubectl get pods"
```

`--interval` is a whole number of seconds and is required. The command is run with `sh -c` on macOS and Linux; the screen is cleared when each run's output starts. Press `Ctrl+C` to stop.

## When a setting does not take effect

| Symptom | Likely cause | What to do |
| --- | --- | --- |
| A `config.toml` change is not visible in the TUI | the TUI reads the file only at startup | restart the TUI |
| A variable you exported changes nothing | the running daemon was started without it | `dot-agent-deck daemon stop`, then relaunch from a shell that has it ([Environment variables](#environment-variables)) |
| `worker_response_timeout_minutes` seems ignored | the key is below a table header, or out of range | move it to the top of the file; use `0`–`10080` ([Top-level keys](#top-level-keys)) |
| The experimental flag stays off | the process read a different `.dot-agent-deck.toml`, or the env var is set to something other than `1`/`true` | check the `experimental flag:` log line ([Turn on experimental features](#turn-on-experimental-features)) |
| The desktop app offers no orchestrations for a directory the TUI handles | the project file is a symlink, not a regular file, or over 1 MiB | make it a regular file under 1 MiB |
| `config set` lost your comments or other keys | `config set` rewrites the file | keep hand edits to the keys above, or edit the file by hand instead of using `config set` |
| Settings changes in the desktop app are not saved | `desktop.toml` cannot be read | read the **Settings** footer, then fix or remove the file |

For other problems, see [Troubleshooting](troubleshooting.md).
