# Telemetry

The deck can send a record of every agent run to an OpenTelemetry backend of your choice: which agent ran in which pane, its role and orchestration, how long it worked, when its status changed, when you stepped in, what kinds of shell commands it ran and how long they took, how long its pull request's CI took, and how it ended. Collected over weeks and across machines, that record answers questions such as which agent or model finishes a kind of task with the least help from you, whether a change to your `CLAUDE.md` or skills made agents stumble less, or how much of a run's time went to tests and CI.

**It is off unless you configure it.** With no OpenTelemetry endpoint set, the deck sends nothing and behaves exactly as it does without this feature. You turn it on with the standard OpenTelemetry environment variables, so a backend that accepts OTLP over HTTP with its credentials in a header works, hosted or your own; [Connections](#connections) has what the deck's connection does and does not support.

The deck sends metadata only. Prompts, file contents, agent output and your API key are never part of it; [What is never sent](#what-is-never-sent) has the full list, the two values it sends exactly as you wrote them (your own service name and resource attributes), and the one thing it reads from a command line: a shell command's program and first word, filtered.

Telemetry behaves the same in the TUI and in the desktop app, because the daemon that runs the agents is what sends it. The only difference between the two clients is where you set the variables.

## Turn it on

Set an endpoint, and the credentials your backend wants as headers, in the environment the **daemon** starts with:

| Variable | What it does |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The backend's OTLP base URL. The deck sends traces to this URL with `/v1/traces` added. Setting it (or the next one) is what turns telemetry on. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | A full URL for traces, used as written. Takes precedence over the one above. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers to send, as `key=value,key2=value2`, usually your API key. If `OTEL_EXPORTER_OTLP_TRACES_HEADERS` is set, the deck sends that list instead, and none of the headers from this one. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` (the default) or `http/json`. `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` overrides it for traces. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | How long one send may take, retries included, in milliseconds (default 10000). |
| `OTEL_SERVICE_NAME` | The service name the deck's data appears under (default `dot-agent-deck`). Sent as you wrote it. |
| `OTEL_RESOURCE_ATTRIBUTES` | Your own `key=value` pairs, added to everything the deck sends, for example `deployment.environment.name=work`. Sent as you wrote them, so keep secrets and private content out of them. |

The deck does not send its own data over `grpc`. With the protocol set to `grpc`, it logs one warning and sends nothing itself; the agents it starts still get the configuration and can use `grpc` if they support it. Point the deck at your backend's HTTP endpoint instead.

### Connections

- **Certificates.** For an `https` endpoint, the deck checks the backend's certificate against the certificates your operating system trusts. A backend whose certificate comes from your own certificate authority works once that authority is installed in the system's trust store.
- **No client certificates.** The deck does not read `OTEL_EXPORTER_OTLP_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE` or `OTEL_EXPORTER_OTLP_CLIENT_KEY`, or their `_TRACES_` forms, so it cannot reach a backend that requires a client certificate (mutual TLS). To send to one, run an [OpenTelemetry Collector](#trim-what-is-sent) on the same machine, point the deck at it over `http://127.0.0.1:4318`, and give the Collector the certificates.
- **No redirects.** If the endpoint answers with a redirect, the deck does not follow it: that send fails, and the log says so. The deck sends your headers to the URL you configured and nowhere else.
- **Retries.** When a send fails because the backend cannot be reached or is busy (HTTP 429, 502, 503 or 504), the deck tries again with growing pauses until `OTEL_EXPORTER_OTLP_TIMEOUT` runs out. A `Retry-After` the backend sends as a number of seconds can make a pause longer but never shorter: the deck waits for whichever is longer, so even `Retry-After: 0` does not bring back-to-back retries. One sent as a date is ignored, and the usual pause applies. Any other answer, and a send still failing at the timeout, drops that batch of spans. When the daemon stops, the deck sends what it still holds before it exits, but only briefly: at most a few seconds, and less when you stop it with `dot-agent-deck daemon stop` while agents are slow to stop, so that the command does not time out. Spans it could not send by then are dropped, and the log says how many (`telemetry: the final flush ran out of time; spans dropped`).
- **Spans the backend turns down.** A backend can accept a send but report that it rejected some of the spans, for example because they were too old. The log then says how many it rejected and the backend's reason (`telemetry: the OTLP backend did not accept every span`); the deck does not send those spans again.

### Where to set the variables

The daemon reads these variables once, when it starts. A daemon that is already running keeps the environment it started with, so after setting or changing them, stop the daemon with `dot-agent-deck daemon stop` and let the next client start a new one. `daemon stop` refuses while agents are running; finish or close them first ([Recycling the daemon](troubleshooting.md#recycling-the-daemon)).

**TUI.** Export the variables in the shell you start `dot-agent-deck` from, or in your shell profile. A TUI that starts a daemon passes its environment to it.

**Desktop app.** A daemon the app starts (with **Start daemon**, or when it restarts the daemon on this machine onto its own version) gets the app's environment, and the app gets your shell's variables only when you launch it from a terminal. An app opened from the macOS Dock or Finder, or from a Linux application menu, does not see variables exported in your shell profile. Pick one:

- Start the daemon yourself from a terminal that has the variables (`dot-agent-deck daemon serve`, or the TUI), then press **Reconnect** in the app ([How the desktop app gets a daemon](installation.md#how-the-desktop-app-gets-a-daemon)).
- Run the daemon as a service and give the variables to the service (next item). The app finds that daemon already running.
- Make the variables part of your login session: on macOS, `launchctl setenv OTEL_EXPORTER_OTLP_ENDPOINT https://api.honeycomb.io` (and the same for each variable), then quit and reopen the app; it lasts until you log out. On Linux desktops whose session takes its environment from systemd, put `KEY=value` lines in a file under `~/.config/environment.d/` and log out and back in. Either way every app you start then sees the variables, your API key included; the service route keeps them to the daemon.

**Daemon run as a service.** A service does not read your shell profile, and `dot-agent-deck daemon install` does not copy these variables into it. Add them the way [Run the Daemon as a Service → Credentials and other environment](daemon-service.md#credentials-and-other-environment) describes: a drop-in from `systemctl --user edit dot-agent-deck.service` on Linux (an `EnvironmentFile=` that only you can read is the better home for the API key), or the `EnvironmentVariables` of the LaunchAgent on macOS. Restarting the service stops its agents.

**Remote hosts and several machines.** Each machine's daemon sends its own runs, from that machine, using its own environment. Set the variables on every machine that should take part, pointing them all at the same backend; each run carries the machine's `host.name`, which is how you tell machines apart. On a remote host, running the daemon as a service with the variables in a drop-in is the dependable way, because it does not depend on how your ssh session sets up its environment.

### Check that it works

Start the daemon with the deck's log on ([Enabling Debug Logs](troubleshooting.md#enabling-debug-logs)). With telemetry on, the log has a line `telemetry: exporting agent-run traces over OTLP` naming the endpoint's host (never your headers). If sending fails, for example because the API key is wrong, the log says `telemetry: OTLP export failed` with the reason, at most once a minute. A failing backend does not affect your agents.

Then start an agent, let it do something, and close it. Spans are sent in batches every few seconds, and a run's `agent.run` span is sent when the run ends, so look for it in the backend after the agent exits or its pane closes.

## Example: Honeycomb's free plan

Honeycomb's free plan is enough for the volume one person's agents produce, and needs nothing installed.

1. Sign up at [honeycomb.io](https://www.honeycomb.io/) and create an **ingest API key** for your environment.
2. Set the variables where the daemon starts (see above). For a shell:

   ```bash
   export OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
   export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=<your API key>"
   ```

   For a Honeycomb team in the EU region, use `https://api.eu1.honeycomb.io`. For a Linux service, the drop-in is:

   ```ini
   [Service]
   Environment=OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
   Environment=OTEL_EXPORTER_OTLP_HEADERS=x-honeycomb-team=<your API key>
   ```

3. Stop the daemon if one is running (`dot-agent-deck daemon stop`), start the deck, and run an agent.

The deck's runs appear in a dataset named `dot-agent-deck` (or your `OTEL_SERVICE_NAME`). Each agent that sends its own telemetry appears under its own service name, for example `claude-code`. Some first queries in the deck's dataset:

- `WHERE name = agent.run`, `GROUP BY deck.agent`, `VISUALIZE HEATMAP(duration_ms)` — how long runs take, per agent.
- `WHERE name = agent.run`, `GROUP BY deck.role`, `VISUALIZE AVG(deck.interventions.count)` — which roles need you most.
- `WHERE name = agent.status AND deck.status = blocked`, `GROUP BY host.name`, `VISUALIZE SUM(duration_ms)` — time spent waiting on a permission prompt, per machine.
- `WHERE name = agent.operation`, `GROUP BY deck.operation.program, deck.operation.word, deck.operation.runner`, `VISUALIZE SUM(duration_ms)` — where agents' shell time goes, for example `cargo test` run locally against the same command in an RWX sandbox.
- `WHERE name = agent.ci AND deck.ci.result exists`, `GROUP BY deck.ci.result`, `VISUALIZE HEATMAP(duration_ms)` — how long CI takes on the pull requests agents open; compare two periods to see what a change to your CI setup did.

To see everything one run did, take its `deck.run.id` from an `agent.run` span and query all datasets in the environment with `WHERE deck.run.id = <that id>`: the deck's spans and the agent's own events come back together.

## What the deck sends

The deck sends traces with four kinds of spans: `agent.run`, one per agent run, and, as children of their run, `agent.status`, one per stretch of time the agent spent in one status, `agent.operation`, one per shell command the agent ran, and `agent.ci`, one per stretch of time the run's pull request had CI checks running.

A run starts when the deck starts an agent in a pane and ends when the agent exits, its pane is closed, or the daemon shuts down. Restarting an agent in the same pane starts a new run with a new id. **A value the deck cannot determine is left out, never guessed and never sent empty.** The deck does not send a span for a run that is still open when the daemon crashes or is killed by force.

### Resource attributes (on everything the deck sends)

| Attribute | Value |
| --- | --- |
| `service.name` | `dot-agent-deck`, or `OTEL_SERVICE_NAME` |
| `service.version` | The deck's version |
| `host.name` | The machine's hostname. Tells several machines apart in one backend. |
| `deck.telemetry.schema_version` | `1`. It changes when the set or meaning of the attributes on this page changes, so data recorded before and after stays comparable. |
| your own | Every pair from `OTEL_RESOURCE_ATTRIBUTES`. Your pairs cannot replace the four above; set `OTEL_SERVICE_NAME` to rename the service. |

### `agent.run`

| Attribute | Value |
| --- | --- |
| `deck.run.id` | A new unique id for this run. The agent gets the same id, which is how its own telemetry joins the run ([below](#agents-own-telemetry)). |
| `deck.pane.id` | The pane the agent ran in. |
| `deck.agent` | The agent: `claude`, `opencode`, `codex`, `pi`, `devin`, or `none` for a command the deck does not recognise as an agent. |
| `deck.role` | The orchestration role, for an agent in an orchestration. |
| `deck.orchestration.name` | The orchestration's name. |
| `deck.orchestration.id` | The id of that run of the orchestration, the same for every role in it. |
| `deck.orchestrator` | `true` for the orchestration's orchestrator, `false` for its other roles. |
| `deck.instructions.commit` | The commit that last changed `CLAUDE.md`, `AGENTS.md` or `.claude/skills` at the root of the agent's repository, whichever directory in it the agent runs from, so you can compare runs before and after an instructions change. Absent outside a git repository or when none of those has any history. |
| `vcs.repository.name` | The repository's name, taken from the last part of its `origin` remote (`dot-agent-deck` for `git@github.com:vfarcic/dot-agent-deck.git`). The remote's URL itself is never sent. |
| `deck.pr.number` | The pull request the agent's branch has, once the deck has found it (the same pull request the card's badge shows). |
| `deck.run.end_reason` | `exited` (the agent exited by itself), `closed` (its pane was closed, or the deck stopped the agent, for example to restart it in its pane), `daemon_shutdown`, or `unknown` when the deck could not tell how the run ended. |
| `deck.status_changes.count` | How many status changes the run had. |
| `deck.interventions.count` | How many times you stepped in (see `deck.intervention` below). |
| `deck.operations.count` | How many shell commands the agent ran (see [`agent.operation`](#agentoperation)). |
| `deck.ci.rounds.count` | How many times the pull request's checks went from running to a result (see [`agent.ci`](#agentci)). |
| `deck.ci.failures.count` | How many of those ended failing. |

Events on the `agent.run` span:

| Event | Attributes | When |
| --- | --- | --- |
| `deck.status_change` | `deck.status.from`, `deck.status.to` | The agent's status changed. The first status of a run has only `deck.status.to`. |
| `deck.intervention` | `deck.intervention.kind` (`input`) | You typed into the agent's pane from either client, or answered its question by voice in the desktop app. Keystrokes less than about five seconds apart count as one intervention. Input the deck sends on its own, such as a delegated task, a message or a scheduled prompt, is not counted, and neither are the replies your terminal sends without you, such as answers to the agent's questions about the cursor position or colours. |
| `deck.ci.change` | `deck.ci.from`, `deck.ci.to`, `deck.pr.number` | The overall result of the pull request's checks changed: `pending`, `passing` or `failing`. The first one for a pull request has only `deck.ci.to`. |

A run with many changes keeps its first 1024 events; the two counts keep counting after that.

**Not sent yet.** A run does not say which model the agent used, the agent's exit code, which issue it worked on, or which skill or command started it. For the model, cost and tokens, use the agent's own telemetry ([Agents' own telemetry](#agents-own-telemetry)). Nor is there an event when an orchestrator hands work to a role; `deck.orchestration.id`, `deck.orchestrator` and `deck.role` tie an orchestration's runs together instead.

### `agent.status`

One span per stretch of time in one status, ending at the next change or with the run. Attributes: `deck.status`, plus `deck.run.id` and `deck.pane.id` so you can query status time without the parent. The statuses are the ones the dashboard shows ([Session Management](session-management.md)): `thinking`, `working`, `compacting`, `waiting`, `idle`, `error`, `blocked` and `unknown`.

### `agent.operation`

One span per shell command the agent ran, from when the agent started it to when it finished, as the agent reports its shell tool calls to the deck. Other tools, such as reading or editing a file, produce no span. A command still running when the run ends ends with the run.

| Attribute | Value |
| --- | --- |
| `deck.operation.runner` | `rwx_sandbox` for a command run with `rwx sandbox exec … -- <command>`, `rwx_run` for `rwx run …`, and `local` otherwise. |
| `deck.operation.program` | The program the command runs, without its directory: `cargo`, `npm`, `gh`. For an RWX sandbox command, the program run inside the sandbox. |
| `deck.operation.word` | The first argument after the program that is not an option: `test` in `cargo test --workspace`, `pr` in `gh pr checks`. |
| `deck.run.id`, `deck.pane.id` | The run, as on `agent.status`. |

The deck reads the first line of the command. When it chains several commands, the first one that does more than set up the next is the one described: `cd`, `export`, `source`, `.`, `set` and `true` are skipped, and so are `VAR=value` assignments and an `env`, `sudo`, `command`, `time` or `nice` in front of the program, with their options and the options' values (`time -f '%e' cargo test` is `cargo test`). When the deck does not know one of those options, as with `env -S`, it sends no program and no word for that command, only where it ran. A command run through `sh -c` or `bash -c` is described by the script it runs, not by the arguments after the script, and a quoted argument stays one argument, so `rwx sandbox exec -- cargo 'two words'` sends no word.

The program and the word are each sent **only when they look like a command word**: lowercase letters, digits, `.`, `_` and `-`, starting with a letter or digit, and at most 32 characters. A path, a URL, a quoted message, anything with an uppercase letter, an `=` or a space, or anything longer is left out. That shape does not make a word safe: a short lowercase password, file name or host name has it too. So for programs whose arguments are usually text, patterns, files, hosts or credentials — `echo`, `printf`, `cat`, `grep`, `rg`, `sed`, `awk`, `curl`, `wget`, `redis-cli`, `base64`, `head`, `tail`, `xargs`, `ssh`, `scp`, `sftp` and `rsync` — the word is never sent, only the program. For any other program the word is its first argument that is not an option, which can be an option's value (`mysql -u admin` sends `admin`); if your agents run programs whose first argument can be private, drop `deck.operation.word` in a Collector ([Trim what is sent](#trim-what-is-sent)).

Which agents report their shell commands:

| Agent | Shell commands become `agent.operation` spans |
| --- | --- |
| Claude Code | Yes, its `Bash` tool. |
| OpenCode | Yes, its `bash` tool. |
| Codex | Yes, its shell tool, through the hooks the deck installs for Codex. |
| Devin | Yes, its `exec` tool. |
| Pi | Yes, its `bash` tool, through the deck's Pi extension. |

### `agent.ci`

One span per stretch of time the run's pull request had CI checks running, from when the deck first saw them `pending` to when it saw them `passing` or `failing`. It comes from the same check of the pull request that puts the CI badge on the agent's card, so it covers the pull request of the agent's branch once the deck has found it.

| Attribute | Value |
| --- | --- |
| `deck.pr.number` | The pull request. |
| `deck.ci.result` | `passing` or `failing`. Absent when the stretch ended for another reason: the run ended first, or the agent's branch moved to another pull request. |
| `deck.ci.checks.passing`, `deck.ci.checks.failing`, `deck.ci.checks.skipped` | How many checks were in each state when the stretch ended. |
| `deck.run.id`, `deck.pane.id` | The run, as on `agent.status`. |

The deck checks the pull request about once a minute (`DOT_AGENT_DECK_PR_REFRESH_SECS` in the daemon's environment changes that), so a stretch's start and end are each up to one check late: compare CI times over many runs rather than reading one to the second. The deck reads the checks as GitHub rolls them up for the pull request, which does not say which CI system ran each one, so to compare two CI setups, compare the periods before and after you switched.

## What is never sent

The deck builds what it sends from the attributes on this page. The values of `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` are the exception to everything below: they are yours, and the deck sends them as you wrote them, so do not put a secret or private content in either. Apart from those, the deck never sends:

- prompts, or anything you type into an agent (an intervention is counted, not recorded);
- file contents, agent output, or the screen of a pane;
- tool-call arguments, or anything else from an agent's tool calls;
- secrets: API keys, the values of environment variables, and the values of your OTLP headers (those go to the endpoint you configured as headers, and to nothing else, and the deck does not write them to its log);
- pane titles, status details, branch names, working-directory paths, remote URLs, or command lines.

**The one exception for command lines** is what [`agent.operation`](#agentoperation) describes: for each shell command, where it ran (`local`, `rwx_sandbox` or `rwx_run`), the name of the program and its first argument. The program and the argument are sent only when they have the shape of a command word (lowercase letters, digits, `.`, `_` and `-`, at most 32 characters), and the first argument of `echo`, `printf`, `cat`, `grep`, `rg`, `sed`, `awk`, `curl`, `wget`, `redis-cli`, `base64`, `head`, `tail`, `xargs`, `ssh`, `scp`, `sftp` and `rsync` is never sent. Nothing else from the command is sent, and the deck's telemetry does not log it. To drop even these, delete the `agent.operation` spans in a Collector ([Trim what is sent](#trim-what-is-sent)).

This covers what the **deck** sends to your backend. The deck's own log on your machine, when you turn it on, is separate from telemetry: it can record details of agents' tool calls, such as the first line of a command an agent ran ([Enabling Debug Logs](troubleshooting.md#enabling-debug-logs)). What an agent sends with its own telemetry is decided by that agent and its settings; see the next section.

## Agents' own telemetry

Agents send their own telemetry too: token use, cost, model, tool decisions. With telemetry on, the deck gives every agent it starts the `OTEL_*` variables the daemon has, unchanged, except `OTEL_RESOURCE_ATTRIBUTES`. That one becomes your own value (if any) plus `deck.run.id`, `deck.pane.id`, `deck.agent`, the orchestration attributes where the run has them, `host.name` and `deck.telemetry.schema_version`. An agent that puts those attributes on its data sends it tagged with the run's `deck.run.id`, and you can join the two in the backend with no manual matching.

For each supported agent the deck also does what that agent needs to send to the same backend, with no setup from you when the deck can find the agent in its command; the Codex and Devin sections say which commands those are. It never turns on an agent's recording of prompts, responses, tool arguments or tool output, and it leaves alone a setting you made yourself, in the environment or in the agent's own configuration.

| Agent | Sends its own telemetry | Joins the deck's run | How to keep it off |
| --- | --- | --- | --- |
| Claude Code | Yes: the deck turns on its metrics and events (see below). | Yes: Claude Code puts `OTEL_RESOURCE_ATTRIBUTES` on every metric and event. | `CLAUDE_CODE_ENABLE_TELEMETRY=0` |
| OpenCode | Yes, whenever `OTEL_EXPORTER_OTLP_ENDPOINT` is set: its logs and traces. | Yes: OpenCode puts `OTEL_RESOURCE_ATTRIBUTES` on what it sends. | Use `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` instead of the general endpoint. |
| Codex | Yes: the deck turns on its traces and metrics; its logs stay off. | Yes. | Name an exporter in Codex's own configuration. |
| Devin | Yes: the deck gives each Devin run a configuration that turns on its events and metrics. | Yes, by the configuration the deck writes; not yet confirmed against a signed-in Devin. | `"enabled": false` in the `otel` block of Devin's configuration, or `OTEL_LOGS_EXPORTER=none` and `OTEL_METRICS_EXPORTER=none`. |
| Pi | Yes: the deck's Pi extension sends Pi's usage. | Yes. | `DOT_AGENT_DECK_PI_TELEMETRY=0` |

Each agent's data appears under its own service name, for example `claude-code` or `pi`. The sections below say how to keep each one off in full; each "keep it off" setting goes in the daemon's environment, or in the agent's own configuration where it says so, and leaves the deck's own spans on.

### Claude Code

With telemetry on and `OTEL_EXPORTER_OTLP_ENDPOINT` set, the deck sets `CLAUDE_CODE_ENABLE_TELEMETRY=1`, `OTEL_METRICS_EXPORTER=otlp`, `OTEL_LOGS_EXPORTER=otlp` and `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` for the Claude Code agents it starts, each one only when you have not set it yourself. Claude Code then sends its metrics (tokens, cost, sessions, lines changed) and events (API requests with model and cost, tool decisions and results) to your backend, tagged with the run's `deck.run.id`. In Honeycomb its events land in a `claude-code` dataset and its metrics in the environment's metrics dataset.

If you set only `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, the deck leaves Claude Code's metrics and events off, because you have not said where they should go. To have the deck turn on just one of them, set its own endpoint, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` or `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`. The protocol the deck sets is `http/protobuf`, the default, even when `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` names another, because that variable is for traces only; set `OTEL_EXPORTER_OTLP_PROTOCOL` yourself to choose the one Claude Code uses.

The deck never turns on Claude Code's content switches (`OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_DETAILS` and the others in [Claude Code's monitoring docs](https://code.claude.com/docs/en/monitoring-usage)), so by default Claude Code sends no prompt text and no tool arguments. If you set one of them yourself in the daemon's environment, the deck passes it through unchanged, and Claude Code then sends that content.

To keep Claude Code's telemetry off while the deck sends its own, set `CLAUDE_CODE_ENABLE_TELEMETRY=0` in the daemon's environment; the deck leaves a value you set alone. To send only one of its signals, set the other exporter to `none` (for example `OTEL_METRICS_EXPORTER=none`). Claude Code's traces stay off unless you turn on its beta tracing as its docs describe.

### OpenCode

OpenCode sends its own logs and traces whenever `OTEL_EXPORTER_OTLP_ENDPOINT` is set (checked against OpenCode 1.19.3), to that URL plus `/v1/logs` and `/v1/traces`, with the headers from `OTEL_EXPORTER_OTLP_HEADERS`, and tagged with `OTEL_RESOURCE_ATTRIBUTES`. It does not read the traces-only endpoint, protocol or timeout variables, so set `OTEL_EXPORTER_OTLP_ENDPOINT` rather than `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` if you want OpenCode's data too, and leave it unset if you do not. Its data appears under the service name `opencode`.

What OpenCode sends does not include token or cost metrics. OpenCode's `experimental.openTelemetry` setting in `opencode.json` adds spans for each model call, which can carry prompt and response text; leave it off unless your backend may hold that. Third-party OpenCode plugins add Claude-Code-style metrics, for example [opencode-plugin-otel](https://github.com/devtheops/opencode-plugin-otel). The deck does not install one; whether a plugin tags its data with `OTEL_RESOURCE_ATTRIBUTES` depends on the plugin, so check its README before relying on the join.

### Codex

Codex sends telemetry only when its configuration names an exporter, so the deck names two when it starts Codex: it adds `-c otel.trace_exporter=…` and `-c otel.metrics_exporter=…` to the Codex command it runs, each pointing at your endpoint for that signal (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/traces` or `/v1/metrics` added) over `http/protobuf`, or `http/json` if that is your protocol. Codex then reads your headers from `OTEL_EXPORTER_OTLP_HEADERS` and tags what it sends with `OTEL_RESOURCE_ATTRIBUTES`, so its traces and metrics join the deck's run. Your headers are never put on the command line. The command your pane shows and saves is the one you configured; the additions are made only to the process the deck starts.

The deck leaves Codex's **logs** off, because Codex's log events include the arguments and part of the output of each tool call. Turn them on yourself with `otel.exporter` in Codex's configuration if your backend may hold that.

The deck finds Codex in your command when the command runs `codex` directly, after `VAR=value` settings, through `env` or `devbox run`, or after the `--` of a wrapper such as `dot-agent-deck wrap --agent codex --`, and puts its additions right after `codex`. A Codex started through a launcher of your own, such as a devbox script or a script with another name, gets no additions; name the exporters in Codex's configuration for it.

The deck adds nothing when your Codex configuration (`config.toml` in `CODEX_HOME`, by default `~/.codex`) already names an exporter in its `[otel]` table, when your Codex command already passes an `otel.` setting, when a signal's exporter variable is set to something other than `otlp` (for example `OTEL_METRICS_EXPORTER=none`, which keeps Codex's metrics off), or when your protocol is `grpc`. It also adds nothing when it cannot tell which configuration Codex will read without running your command: a `CODEX_HOME` written with `$`, `~` or a command substitution, one that is not an absolute path, one set by `export` or by a file the command sources, or an `env -i` or `env -u` in front of Codex. Write `CODEX_HOME=/absolute/path codex` in the command, or set `CODEX_HOME` in the daemon's environment, for the deck to read it. To keep Codex's telemetry off entirely, put `trace_exporter = "none"` and `metrics_exporter = "none"` in the `[otel]` table of Codex's configuration.

**Keep credentials out of the endpoint URL.** The endpoint goes on Codex's command line, which other processes on your machine can read, so the deck adds nothing for an endpoint with a `user:password@` part or a query string. Put credentials in `OTEL_EXPORTER_OTLP_HEADERS`, which Codex reads from its environment; an endpoint whose path carries a token cannot be told apart, so do not use one.

### Devin

Devin sends telemetry only when its user configuration names an endpoint, and it does not read `OTEL_RESOURCE_ATTRIBUTES`. So for each Devin run the deck writes a copy of your Devin configuration (`config.json` under `$XDG_CONFIG_HOME/devin` or `~/.config/devin`) with an `otel` block added, and starts Devin with `--config` pointing at it. The block holds your endpoint, the run's resource attributes (the deck's and your own), and `disable_user_prompt_logging` and `disable_tool_logging` set to `true`, because Devin otherwise records prompts, responses and tool arguments. Devin then sends its events and metrics over `http/protobuf`, with your headers from `OTEL_EXPORTER_OTLP_HEADERS`; it sends no traces, so a traces-only endpoint gives Devin nothing to send. Devin does not read `OTEL_LOGS_EXPORTER` or `OTEL_METRICS_EXPORTER` itself, so the deck does: with `OTEL_LOGS_EXPORTER=none` the block names only the metrics endpoint, with `OTEL_METRICS_EXPORTER=none` only the events (logs) endpoint, and with both the run gets no block.

The deck adds no credentials to the copy, but it is a copy of your Devin configuration and contains whatever that contains. Only you can read it, it lives in a directory of the deck's state directory that only you can enter (the deck refuses to write it when that directory is a symlink or belongs to someone else), and the deck removes it when the run ends, or, if the daemon crashed or was killed first, when the daemon next starts. The deck finds Devin in your command the same ways as Codex.

If your Devin configuration already has an `otel` block, the deck keeps it exactly as you wrote it and adds only the resource attributes it lacks. To keep Devin's telemetry off, put `"enabled": false` in that block, and the deck starts Devin as usual. Nothing is added either when your Devin command already passes `--config`, or when your protocol is `grpc`.

That Devin's data carries the run's `deck.run.id` this way follows from Devin's own documentation of its configuration; it has not yet been checked against a signed-in Devin. If its data in your backend has no `deck.run.id`, the deck's `agent.run` span (agent `devin`, pane, start and end time) is what you have for those runs.

### Pi

Pi has no OpenTelemetry export of its own, so the deck's Pi extension, which the deck already installs for Pi, sends it. With telemetry on and a logs endpoint (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`, or `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/logs` added), it sends one log record per event under the service name `pi`, as OTLP/JSON with your headers, tagged with the run's resource attributes:

- `pi.model_usage` for each reply from the model: the model, the provider, the input, output and cache token counts, and the cost Pi estimates;
- `pi.tool_result` for each tool call: the tool's name, whether it succeeded, and how long it took;
- `pi.session_start`, `pi.agent_start`, `pi.agent_end` and `pi.session_end`.

It never sends a prompt, a reply, a tool's arguments or a tool's output. It sends in batches every few seconds and when Pi finishes a turn or exits, and a backend that is slow or down does not affect Pi: when Pi exits, it waits at most two seconds for the last batch. Like the deck, it does not follow redirects, so your headers go to the endpoint you configured and nowhere else. To keep it off, set `DOT_AGENT_DECK_PI_TELEMETRY=0` in the daemon's environment, or `OTEL_LOGS_EXPORTER=none`, which also keeps Claude Code's events off.

## Trim what is sent

The deck sends everything on this page on purpose: while it is not yet clear which signals matter, it is easier to drop a field later than to wish you had it. Trim in your backend or on the way to it, without changing the deck.

Most backends let you ignore or hide a field in queries. To stop a field or span from being stored at all, send the deck's data through an [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/) and drop it there. A Collector on the same machine, forwarding to Honeycomb, with the deck pointed at it (`OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318`):

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 127.0.0.1:4318

processors:
  # Drop span attributes you do not want stored.
  attributes/trim:
    actions:
      - key: vcs.repository.name
        action: delete
      - key: deck.orchestration.name
        action: delete
  # Drop whole spans: here, the per-status and per-command spans.
  filter/trim:
    error_mode: ignore
    traces:
      span:
        - 'name == "agent.status"'
        - 'name == "agent.operation"'

exporters:
  otlphttp/honeycomb:
    endpoint: https://api.honeycomb.io
    headers:
      x-honeycomb-team: ${env:HONEYCOMB_API_KEY}

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [attributes/trim, filter/trim]
      exporters: [otlphttp/honeycomb]
```

`host.name` and the other resource attributes are dropped with the Collector's `resource` processor instead of `attributes`. Agents pointed at the same Collector need `logs` and `metrics` pipelines as well for their own data.

## Turn it off

Any one of these turns the deck's telemetry off, from the daemon's next start:

- unset `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`;
- set `OTEL_SDK_DISABLED=true`;
- set `OTEL_TRACES_EXPORTER=none`.

With it off, the deck sends nothing and adds nothing to how it starts agents: no variables, no Codex or Devin additions, and the Pi extension stays quiet, so each agent's telemetry goes back to whatever you set yourself. An agent that reads the endpoint on its own, such as OpenCode, keeps sending while the endpoint is still set, so unsetting the endpoint is the way to stop everything. Then stop the daemon (`dot-agent-deck daemon stop`) so the next one starts without it.
