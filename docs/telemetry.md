# Telemetry

The deck can send a record of every agent run to an OpenTelemetry backend of your choice: which agent ran in which pane, its role and orchestration, how long it worked, when its status changed, when you stepped in, what kinds of shell commands it ran and how long they took, how long its pull request's CI took, how many tokens it used on which model, and how it ended. Collected over weeks and across machines, that record answers questions such as which agent or model finishes a kind of task with the least help from you, whether a change to your `CLAUDE.md` or skills made agents stumble less, or how much of a run's time went to tests and CI.

**It is off unless you configure it.** With no OpenTelemetry endpoint set, the deck sends nothing and behaves exactly as it does without this feature. You turn it on with the standard OpenTelemetry environment variables, so a backend that accepts OTLP over HTTP with its credentials in a header works, hosted or your own; [Connections](#connections) has what the deck's connection does and does not support.

The deck sends metadata only. Prompts, file contents, agent output and your API key are never part of it; [What is never sent](#what-is-never-sent) has the full list, the two values it sends exactly as you wrote them (your own service name and resource attributes), and the one thing it reads from a command line: the program and first word of each command in a shell command line, filtered.

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
- **Spans the backend turns down.** A backend can accept a send but report that it rejected some of the spans, for example because they were too old. The log then says how many it rejected (`telemetry: the OTLP backend did not accept every span`), not why: for the reason, look at your Collector's or backend's own logs or diagnostics. The deck does not send those spans again.

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

Start the daemon with the deck's log on ([Enabling Debug Logs](troubleshooting.md#enabling-debug-logs)). With telemetry on, the log has a line `telemetry: exporting agent-run traces over OTLP` naming the endpoint's host (never your headers). If sending fails, for example because the API key is wrong, the log says `telemetry: OTLP export failed` with the HTTP status or the kind of failure, at most once a minute; it does not repeat what the backend wrote back. A failing backend does not affect your agents.

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
- `WHERE name = agent.operation`, `GROUP BY deck.operation.commands`, `VISUALIZE SUM(duration_ms)` — where agents' shell time goes, for example how much of it `cargo test` takes against `cargo build`. Grouping by `deck.operation.commands` rather than by `deck.operation.program` and `deck.operation.word` keeps `cargo fmt && cargo test` apart from a plain `cargo fmt`.
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
| `deck.telemetry.schema_version` | `2`. It changes when the meaning of an attribute on this page changes, so you can keep data recorded before and after apart. A new attribute does not change it. |
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
| `deck.run.exit_code` | The exit status of an agent or command that exited by itself. Absent otherwise: when a signal ended it, when its pane was closed or the daemon stopped, or when the deck could not read the status. |
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

A run also says what it used and how it ended, with one more event per model: [What a run used and how it ended](#what-a-run-used-and-how-it-ended).

**Not sent yet.** A run does not say which issue it worked on, or which skill or command started it. Nor is there an event when an orchestrator hands work to a role; `deck.orchestration.id`, `deck.orchestrator` and `deck.role` tie an orchestration's runs together instead.

### What a run used and how it ended

Each `agent.run` span says how many tokens the run used on which model, how much of a plan's allowance it took, what it would cost, and how the agent's own work ended. The figures are the agent's own: the deck reads them from the record each agent keeps of its usage on your machine, so they need no extra setup and are sent only from the deck, with the run they belong to. Neither client shows them; they are in your backend only.

**Every value says where it came from**, in a `.provenance` attribute beside it:

| Provenance | Meaning |
| --- | --- |
| `provider_reported` | The agent's own count, as its provider reported it. |
| `estimated` | Worked out from other values. A cost is always this: what the tokens would cost at the agent's list price. **It is never what you were charged**, and on a subscription it is not what the run cost you. |
| `measured` | Seen by the deck itself, such as an exit status. |
| `unavailable` | Not known. The value itself is left out, and `.unavailable_reason` says why. |

**A value that is not known is never sent as `0`.** A `0` you see is one the agent counted. The reasons a value can be unavailable:

| Reason | Meaning |
| --- | --- |
| `no_channel` | The deck has no way to learn this value for this agent (see the table below). |
| `subscription_zero` | The agent reported a cost of 0 for work that used tokens, which is what a subscription provider reports. The run's cost is not knowable. |
| `unknown_price` | The agent did not know the price of a model the run used. |
| `not_recorded` | The agent kept no record of it for this run, for example because it was killed before writing its totals, or because the deck's Pi extension or OpenCode plugin was not loaded. |
| `incomplete` | Part of the value was known and part was not, so a total would have been a part presented as the whole: for example, one of the run's sessions had a record the deck could not read, or the agent stopped in the middle of writing it. |
| `unrecognized_format` | The agent's record is in a format the deck does not know, as can happen after an agent upgrade. |
| `window_reset`, `plan_changed`, `quota_decreased` | Codex's allowance window reset, its plan changed, or its used share went down during the run, so the share the run used cannot be worked out. |

On the `agent.run` span:

| Attribute | Value |
| --- | --- |
| `gen_ai.usage.input_tokens` | The run's input tokens across all models, including tokens read from and written to the provider's cache. |
| `gen_ai.usage.output_tokens` | The run's output tokens across all models. |
| `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens` | The parts of the input read from and written to the cache. |
| `deck.tokens.provenance`, `deck.tokens.unavailable_reason` | Where the token counts came from, or why they are unavailable. |
| `deck.cost.microusd` | The run's cost estimate across all models, in millionths of a US dollar: `12300` is $0.0123. |
| `deck.cost.provenance`, `deck.cost.unavailable_reason` | `estimated`, or why the cost is unavailable. |
| `deck.quota.plan` | Codex's plan (`plus`, `pro`, `team` and the others Codex reports), or `other` for a plan the deck does not know. |
| `deck.quota.window_minutes`, `deck.quota.resets_at` | The length of Codex's allowance window, and when it resets, in seconds since 1970. |
| `deck.quota.used_from_bp`, `deck.quota.used_to_bp` | The share of the window's allowance used when the run's first and last figures arrived, in hundredths of a percent: `1200` is 12%. The difference is what the run used. |
| `deck.quota.provenance`, `deck.quota.unavailable_reason` | Where the allowance figures came from, or why they are unavailable. |
| `deck.run.termination` | How the run ended: `completed`, `failed`, `interrupted` or `unknown`. |
| `deck.run.termination.provenance` | `provider_reported` when the agent's own record of its last turn decided it, `measured` when the deck did. |
| `deck.run.last_turn`, `deck.run.last_turn.provenance` | How the agent's last turn ended, by its own record: `completed`, `failed`, `interrupted`, `mid_turn` or `unknown`. An interactive agent completes a turn and then waits, so this is kept beside the termination. `unavailable` with `not_recorded` when the agent kept no record. |
| `error.type` | For a failed run, the kind of failure, such as `rate_limit` or `server_overloaded`, from a list of kinds the agents are known to report, or `other`. For a command, `nonzero_exit`. Never the error's message. |
| `http.response.status_code` | The HTTP status of the provider's answer that failed the run, when the agent recorded one. |

Events on the `agent.run` span, one `deck.model_usage` per model the run used:

| Attribute | Value |
| --- | --- |
| `gen_ai.response.model` | The model, such as `claude-haiku-4-5-20251001` or `gpt-5.4-mini`. Left out for a model whose name the deck does not recognise as a known model's, such as a custom or local model, a name with a path or a prefix of your own in front of it, or one with a suffix of your own after it, such as an address: its tokens are still sent. |
| `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens` | That model's tokens, as on the run. |
| `deck.tokens.provenance`, `deck.tokens.unavailable_reason`, `deck.cost.microusd`, `deck.cost.provenance`, `deck.cost.unavailable_reason` | That model's provenance and cost estimate, as on the run. |

A run with no usage known at all has one `deck.model_usage` event without a model, its tokens and cost unavailable with the reason. A run that used models of different provenance, such as one model with a cost estimate and one without, has its total `unavailable` as `incomplete`; the per-model events still carry what is known.

Where each agent's figures come from:

| Agent | Tokens | Cost | Allowance | How it ended |
| --- | --- | --- | --- | --- |
| Claude Code | Claude Code's own session total, which it writes when it exits. End a Claude Code run with `/exit`: one that is killed, or whose pane is closed while Claude Code runs, can have its tokens `not_recorded`. A run that resumes an earlier session counts only what it added, or is `incomplete` when the deck cannot tell what that was. | Claude Code's own estimate. | `no_channel` | Claude Code's last turn, with the kind of an API error and its HTTP status. |
| Codex | Codex's own running total, as the run goes. | `no_channel`: Codex reports no cost. | Codex's plan and its main allowance window. **The used share is the plan's, not the run's alone**: runs on the same plan whose times overlap share one window, so a run's figure includes their use too. The first figure arrives with the run's first reply, so the run's first request is not in it. So the difference between the two figures is not an exact count of what the run took from the allowance. | Codex's last turn, with its error kind. |
| OpenCode | Each reply's tokens, as OpenCode hands them to the deck's OpenCode plugin. | OpenCode's own figure. Subscription providers report 0, which is sent as `unavailable` with `subscription_zero`. | `no_channel` | OpenCode's last reply. |
| Pi | Each reply's tokens, as Pi hands them to the deck's Pi extension. | Pi's own estimate. | `no_channel` | Pi's last reply. |
| Devin | `no_channel` | `no_channel` | `no_channel` | `unknown`, `measured` |
| A command the deck does not recognise as an agent | `no_channel` | `no_channel` | `no_channel` | Its exit status, `measured`: `completed` for 0, `failed` with `error.type` `nonzero_exit` for any other status, `interrupted` when you closed its pane or the daemon stopped, `unknown` when a signal ended it. |

**Devin usage is unavailable: its reported token counts have not been verified against a signed-in session; missing values are not zero.**

How the run ended, for an agent: the agent's own last turn decides when it completed, failed or was interrupted (`provider_reported`). When its record stops in the middle of a turn, a run you closed or that the daemon stopped is `interrupted`, and one that ended any other way is `unknown` (`measured`). So an agent that exits by itself after finishing its work is `completed`, and a crash in the middle of a turn is `unknown`, not `failed`.

The OpenCode and Pi figures arrive through the plugin and extension the deck installs for them, and only while telemetry is on. An OpenCode or Pi started before you updated the deck, or one that did not load them, has its usage `not_recorded`.

**An agent started inside another agent's pane**, as a program of that pane rather than in a pane of its own, has no run of its own: its usage is added to that pane's run when it is the same agent, and is not recorded when it is another one. An agent the deck starts in a new pane has its own run, whichever pane asked for it.

### `agent.status`

One span per stretch of time in one status, ending at the next change or with the run. Attributes: `deck.status`, plus `deck.run.id` and `deck.pane.id` so you can query status time without the parent. The statuses are the ones the dashboard shows ([Session Management](session-management.md)): `thinking`, `working`, `compacting`, `waiting`, `idle`, `error`, `blocked` and `unknown`.

### `agent.operation`

One span per shell command the agent ran, from when the agent started it to when it finished, as the agent reports its shell tool calls to the deck. Other tools, such as reading or editing a file, produce no span. A command still running when the run ends ends with the run.

| Attribute | Value |
| --- | --- |
| `deck.operation.program` | The program the command runs, without its directory: `cargo`, `npm`, `gh`. |
| `deck.operation.word` | The first argument after the program, only when no option comes before it: `test` in `cargo test --workspace`, `pr` in `gh pr checks`, and nothing in `git -C repo status`. |
| `deck.operation.commands` | Every command in the line, in order, each as its program or its program and word with a space between, joined with `,`: `cargo fmt,cargo xtask` for `cargo fmt && cargo xtask affected-checks --run`, and `cargo test` for a line of one command. At most 8 commands. Absent when the first command is recorded without a program. |
| `deck.operation.wrapped.program`, `deck.operation.wrapped.word` | The program and word of the command a wrapper runs, when the first command is one of the wrappers listed below: `cargo` and `test` for `timeout 600 cargo test`. |
| `deck.run.id`, `deck.pane.id` | The run, as on `agent.status`. |

The span does not say whether the command succeeded. Some agents send that with their own telemetry, joined to the run by `deck.run.id`: Claude Code's tool-result events and Pi's `pi.tool_result` say whether each tool call succeeded ([Agents' own telemetry](#agents-own-telemetry)). Claude Code does not tell the deck when a command fails, so the span of a command that failed in Claude Code can last until the run ends.

`deck.operation.program` and `deck.operation.word` describe the first command on the first line that does more than set up the next one, so `cd repo && cargo test` is `cargo test`, and it looks past `NAME=value` settings and a wrapper such as `env`, `sudo` or `time` in front of the program. **It sends the program and the word only for plain commands.** For the agents that report a command as it was typed (all of them but Codex), up to and including the first argument the command must be plain words made of letters, digits and `/ . _ + : , @ % -`, plus `NAME=value` settings with such a value. A command that uses other shell syntax before or in its first argument, such as quotes, a `$` variable or `$(…)`, backticks, a comment, brackets or braces, a redirection, `~` or a wildcard, is recorded without a program or word: only the span and its duration. That includes the programs listed below whose first argument is never sent, such as `grep`. So is a command whose wrapper options the deck does not know, and one whose command is shell grammar rather than a program: a reserved word of sh, bash or zsh other than `time` and `export`, which the deck looks past, such as `if`, `for`, `case` or zsh's `foreach` and `repeat`; a declaration such as `local` or `typeset`; a modifier such as `noglob` or `builtin`; or `eval` or `exec`. A shell that reads its script from standard input, such as `bash -s`, is the exception for the script's arguments: the deck sends the shell's name with no word and does not look at those arguments, so `bash -s '$X'` still sends `bash`. The shell's own options must still be plain, as for any command. What comes after the first argument is not read, so `cargo test 2>&1 | tail` is still `cargo test`, and a command run as `bash -c 'cargo test'` is described by the plain command inside the quotes. For Codex the deck sends the program only, and only when the word that names the program is itself plain; the table below says why.

**Chained commands.** An agent often runs several commands in one line, joined with `&&`, `||`, `;` or `|`. The span lasts for the whole line, and the agent does not report how long each command in it took, so `deck.operation.commands` says which commands shared that time rather than splitting it. Each command in the line is described by the same rules as the first, and commands that only set up the next one, such as `cd`, are left out. The list stops at the first command that would be recorded without a program, or a setup command that uses other shell syntax, such as `cd "$DIR"`, and nothing after it is read. It also stops after a command that has anything after its first argument other than plain words (a `=` in them is fine), text in one pair of quotes with no quote, `$`, backtick or `\` inside, and `2>&1`-style redirections to another output or to `/dev/null`: a `$` variable, a comment, a `>` to a file or a lone `&` ends it. So `cargo fmt && cargo test` sends `cargo fmt,cargo test`, `git commit -m 'Fix it' && git push` sends `git commit,git push`, and `cargo fmt > out.txt && cargo test` sends `cargo fmt`. The quoted message, like everything else after a command's first argument, is never sent.

**Wrapped commands.** For a first command that runs another command, `deck.operation.wrapped.program` and `deck.operation.wrapped.word` describe the command it runs, by the same rules. This works only for these wrappers, in these forms:

| Wrapper | Where the command it runs starts |
| --- | --- |
| `timeout` | After its options and the duration: `timeout 600 cargo test`. |
| `nohup` | Straight after it: `nohup cargo build`. |
| `docker exec` | After its options and the container: `docker exec -it box make ci`. An option given as `NAME=value`, such as `-e FOO=1`, sends no wrapped command. |
| `kubectl exec` | After `--`: `kubectl exec -it pod -- make ci`. |
| `nix develop`, `nix shell` | After `-c` or `--command`: `nix develop -c cargo test`. |
| `devbox run` | After a `--` that has only options before it: `devbox run -- cargo test`. |

For any other wrapper, such as `uv run pytest` or `poetry run pytest`, the span describes the wrapper alone, as it did before. A wrapper whose options the deck does not know, one given `-h`, `--help` or `--version`, or one with quotes or other shell syntax before the command it runs, sends no wrapped command. Prefixes such as `env`, `sudo` and `time`, which the deck already looks past, are not wrappers: `sudo cargo test` is still `cargo test` in `deck.operation.program`.

The program and the word are each sent **only when they look like a command word**: lowercase letters, digits, `.`, `_` and `-`, starting with a letter or digit, and at most 32 characters. A path, a URL, a quoted message, anything with an uppercase letter, an `=` or a space, or anything longer is left out. That shape does not make a word safe: a short lowercase password, file name or host name has it too. So for programs whose arguments are usually text, patterns, files, hosts or credentials — `echo`, `printf`, `cat`, `grep`, `rg`, `sed`, `awk`, `curl`, `wget`, `redis-cli`, `base64`, `head`, `tail`, `xargs`, `ssh`, `scp`, `sftp` and `rsync` — the word is never sent, only the program. For any other program the word is its first argument, and only when no option comes before it, since an option can take the next argument as its value: `sshpass -p <password> ssh host` and `mysql -u admin db` send the program and no word. These rules apply to every command in `deck.operation.commands` and to the wrapped command, so a later command's first word, or a wrapped command's, is sent when the first command's would be. If your agents run programs whose first argument can be private, drop `deck.operation.word`, `deck.operation.commands` and `deck.operation.wrapped.word` in a Collector ([Trim what is sent](#trim-what-is-sent)); dropping `deck.operation.word` alone no longer covers it, since `deck.operation.commands` holds the same word.

Which agents report their shell commands:

| Agent | Shell commands become `agent.operation` spans |
| --- | --- |
| Claude Code | Yes, its `Bash` tool. |
| OpenCode | Yes, its `bash` tool. |
| Codex | Yes, its shell tool, through the hooks the deck installs for Codex. Codex does not report where each argument of a command ends, so for Codex the deck sends the program only, never a word, and no program either when the word naming it is not plain or it is not certain which word is the program. For the same reason Codex spans name one command at most: `deck.operation.commands` holds that program alone, and there is no wrapped command. |
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
- pane titles, status details, branch names, working-directory paths, remote URLs, or command lines;
- an agent's error messages: a failed run carries the kind of failure from a known list, or `other`;
- names you chose: a model name is sent only when the deck recognises it as a known model's name, and a Codex plan only from the plans Codex is known to report.

The [collector](#cpu-memory-and-disk-per-run), a separate program you run yourself, sends a worktree's path only when you start it with `--send-worktree-paths`; [What it sends](#what-it-sends) has its list.

**The one exception for command lines** is what [`agent.operation`](#agentoperation) describes: for each command in a shell command line, and for the command a listed wrapper runs, at most the name of the program and its first argument (for Codex, the program of one command only), and only when they pass the rules that section gives. Nothing else from the command line is sent, and the deck's telemetry does not log it. To drop even these, delete the `agent.operation` spans in a Collector ([Trim what is sent](#trim-what-is-sent)).

This covers what the **deck** sends to your backend. The deck's own log on your machine, when you turn it on, is separate from telemetry: it can record details of agents' tool calls, such as the first line of a command an agent ran ([Enabling Debug Logs](troubleshooting.md#enabling-debug-logs)). What an agent sends with its own telemetry is decided by that agent and its settings; see the next section.

## Agents' own telemetry

Agents send their own telemetry too: token use, cost, model, tool decisions. With telemetry on, the deck gives every agent it starts the `OTEL_*` variables the daemon has, unchanged, except `OTEL_RESOURCE_ATTRIBUTES`. That one becomes your own value (if any) plus `deck.run.id`, `deck.pane.id`, `deck.agent`, the orchestration attributes where the run has them, `host.name` and `deck.telemetry.schema_version`. An agent that puts those attributes on its data sends it tagged with the run's `deck.run.id`, and you can join the two in the backend with no manual matching.

The deck also switches on each supported agent's own telemetry, with no setup from you, for the signals your configuration gives a destination: an agent sends a signal only when you have set an endpoint it reads for that signal. With only `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` set, for example, Codex sends its traces and the other agents send nothing of their own. The table below says what each agent sends, and its section what it needs. This works whether you start the agent directly in a pane or through a script or launcher that finds it on your `PATH`, such as `devbox run`; the Codex section says what that does not reach. It never turns on an agent's recording of prompts, responses, tool arguments or tool output, and it leaves alone a setting you made yourself, in the environment or in the agent's own configuration.

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

Codex sends telemetry only when its configuration names an exporter, so the deck names two when it starts Codex: it adds `-c otel.trace_exporter=…` and `-c otel.metrics_exporter=…` to the Codex command it runs, each pointing at your endpoint for that signal (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/traces` or `/v1/metrics` added) over `http/protobuf`, or `http/json` if that is your protocol. Codex then reads your headers from `OTEL_EXPORTER_OTLP_HEADERS` and tags what it sends with `OTEL_RESOURCE_ATTRIBUTES`, so its traces and metrics join the deck's run. Your headers are never put on the command line. The command your pane shows and saves is the one you configured; the additions go in front of your own arguments, on the Codex process only.

The deck leaves Codex's **logs** off, because Codex's log events include the arguments and part of the output of each tool call. Turn them on yourself with `otel.exporter` in Codex's configuration if your backend may hold that.

The deck switches on Codex's telemetry however Codex is started, as long as it is found by name on `PATH`: directly in a pane, through `devbox run`, a script of your own that runs `codex`, or another launcher. With telemetry on, the deck puts a directory of its own, `telemetry/bin` in the deck's state directory, first on the `PATH` of every agent it starts. It holds a `codex` and a `devin` that switch on that agent's telemetry and then run the real one, the next one on your `PATH`, with your arguments unchanged; the program's exit status, signals and terminal are its own. What is added is decided from the environment Codex really starts with, after your script or launcher has run, so a script that sets `CODEX_HOME` or exports `OTEL_TRACES_EXPORTER=none` before running `codex` is respected. If anything goes wrong while deciding, Codex starts without the additions. You will see the directory at the front of `PATH` inside the deck's panes.

Because that directory is on the `PATH` of every agent the deck starts, a Codex or Devin that another agent starts inside a pane, for example Claude Code running `codex exec`, has its telemetry switched on too, and inherits that pane's resource attributes, so what it sends carries that pane's `deck.run.id`. A Codex started from inside another Codex that the deck's directory started is left as it is, and so is a Devin started from inside another such Devin.

This works as long as the deck's `codex` is the first `codex` found on `PATH` when Codex starts. A script that runs Codex by its full path (for example `/usr/local/bin/codex`), sets `PATH` to a new value, or puts a directory holding the real Codex in front of the deck's (for example `export PATH=/opt/codex/bin:$PATH` before running `codex`) runs the real Codex directly, so it gets nothing. The deck also adds nothing when the agent starts with no `PATH` variable at all, in the daemon's environment or its own.

The deck switches on a `codex` installed in one of the standard ways: Codex's own program, the launcher npm installs for it, or the one Nix or devbox installs. Any other `codex` found first on `PATH`, such as a script of your own, in any language, that sets things up and then runs the real Codex, is left alone, and Codex starts without the additions. If you use a customised launcher in one of those standard places that changes Codex's configuration once it has started, for example by setting `CODEX_HOME` or an `OTEL_` variable, the deck may switch on telemetry your launcher meant to change. For such a launcher, set telemetry up in Codex's own configuration, or start Codex by your launcher's full path, which bypasses the deck's switch-on. On Windows the deck does not switch on Codex's or Devin's telemetry. For those, name the exporters in Codex's own configuration.

The deck adds nothing when your Codex configuration (`config.toml` in `CODEX_HOME`, by default `~/.codex`) already names an exporter in its `[otel]` table, when Codex's arguments already set something under `otel` with `-c` or `--config`, when `OTEL_SDK_DISABLED=true`, when `CODEX_HOME` (or `HOME`, when `CODEX_HOME` is not set) is not an absolute path, or when your protocol is `grpc`. A signal whose exporter variable is set to something other than `otlp` gets nothing for that signal, and the other signal is switched on as usual: `OTEL_METRICS_EXPORTER=none`, for example, keeps Codex's metrics off and still turns on its traces. To keep Codex's telemetry off entirely, put `trace_exporter = "none"` and `metrics_exporter = "none"` in the `[otel]` table of Codex's configuration.

**Keep credentials out of the endpoint URL.** The endpoint goes on Codex's command line, which other processes on your machine can read, so the deck adds nothing for an endpoint with a `user:password@` part or a query string. Put credentials in `OTEL_EXPORTER_OTLP_HEADERS`, which Codex reads from its environment; an endpoint whose path carries a token cannot be told apart, so do not use one.

### Devin

Devin sends telemetry only when its user configuration names an endpoint, and it does not read `OTEL_RESOURCE_ATTRIBUTES`. So for each Devin run the deck writes a copy of your Devin configuration (`config.json` under `$XDG_CONFIG_HOME/devin` or `~/.config/devin`) with an `otel` block added, and starts Devin with `--config` pointing at it. The block holds your endpoint, the run's resource attributes (the deck's and your own), and `disable_user_prompt_logging` and `disable_tool_logging` set to `true`, because Devin otherwise records prompts, responses and tool arguments. Devin then sends its events and metrics over `http/protobuf`, with your headers from `OTEL_EXPORTER_OTLP_HEADERS`; it sends no traces, so a traces-only endpoint gives Devin nothing to send. Devin does not read `OTEL_LOGS_EXPORTER` or `OTEL_METRICS_EXPORTER` itself, so the deck does: with `OTEL_LOGS_EXPORTER=none` the block names only the metrics endpoint, with `OTEL_METRICS_EXPORTER=none` only the events (logs) endpoint, and with both the run gets no block.

The deck does not copy your OTLP header variables into the file, but it is a copy of your Devin configuration and contains whatever that contains. Only you can read it, it lives in a directory of the deck's state directory that only you can enter (the deck refuses to write it when that directory is a symlink or belongs to someone else), and the deck removes it when the run ends, or, if the daemon crashed or was killed first, when a daemon next starts after that Devin has exited. The deck reaches Devin the same way as Codex, with the same limits; the `devin` it switches on is Devin's own program or a script Nix or devbox installed, as for Codex. It reads `XDG_CONFIG_HOME`, `HOME` and the exporter variables from the environment Devin really starts with, so a script that sets `XDG_CONFIG_HOME=/other` before running `devin` gets a copy of `/other/devin/config.json`. Each Devin started gets its own file, and all of a run's files are removed when the run ends.

If your Devin configuration already has an `otel` block, the deck keeps it exactly as you wrote it, endpoint included, and adds only the resource attributes it lacks. To keep Devin's telemetry off, put `"enabled": false` in that block, and the deck starts Devin as usual. Nothing is added either when Devin's arguments already pass `--config`, when `OTEL_SDK_DISABLED=true`, or when your protocol is `grpc`. When the deck would write a new `otel` block, it writes none for an endpoint with a `user:password@` part or a query string, as for Codex.

That Devin's data carries the run's `deck.run.id` this way follows from Devin's own documentation of its configuration; it has not yet been checked against a signed-in Devin. If its data in your backend has no `deck.run.id`, the deck's `agent.run` span (agent `devin`, pane, start and end time) is what you have for those runs.

### Pi

Pi has no OpenTelemetry export of its own, so the deck's Pi extension, which the deck already installs for Pi, sends it. With telemetry on and a logs endpoint (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`, or `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/logs` added), it sends one log record per event under the service name `pi`, with your headers, tagged with the run's resource attributes:

- `pi.model_usage` for each reply from the model: the model, the provider, the input, output and cache token counts, and the cost Pi estimates;
- `pi.tool_result` for each tool call: the tool's name, whether it succeeded, and how long it took;
- `pi.session_start`, `pi.agent_start`, `pi.agent_end` and `pi.session_end`.

Pi's records are always sent as OTLP/JSON (`application/json`) to the logs endpoint, even when `OTEL_EXPORTER_OTLP_PROTOCOL` or `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` is `http/protobuf`, the default, so your backend must accept OTLP/JSON for them; with `grpc` the deck does not switch Pi's export on. The OTLP specification recommends that a backend accept both JSON and binary Protobuf but does not require it; Honeycomb and the OpenTelemetry Collector accept both. It never sends a prompt, a reply, a tool's arguments or a tool's output. It sends in batches every few seconds and when Pi finishes a turn or exits, and a backend that is slow or down does not affect Pi: when Pi exits, it waits at most two seconds for the last batch. Like the deck, it does not follow redirects, so your headers go to the endpoint you configured and nowhere else. To keep it off, set `DOT_AGENT_DECK_PI_TELEMETRY=0` in the daemon's environment, or `OTEL_LOGS_EXPORTER=none`, which also keeps Claude Code's events off. `OTEL_SDK_DISABLED=true` or `OTEL_LOGS_EXPORTER=none` in one Pi agent's own environment, for example set by a script that starts Pi, keeps that agent's telemetry off.

## CPU, memory and disk per run

The deck's spans say what each run did, not what it cost the machine. A separate program, `dot-agent-deck-collector`, adds that: every 15 seconds it sends each run's CPU time, memory and number of processes, and every 5 minutes the disk space of each worktree a run works in, as OpenTelemetry metrics to the same backend as the deck's spans. Each run's figures carry the run's `deck.run.id`, so in the backend they sit next to its `agent.run` span and the agent's own telemetry. On a busy machine, that tells you which run is burning CPU, which is holding memory and which worktree is filling the disk.

The collector runs on its own, beside the deck rather than inside it. You start it, one per machine, and it covers every deck you run there as the same user; the deck does not start it, and it changes nothing about how the deck or its agents behave. Its figures appear in your backend only: the TUI and the desktop app do not show them.

**It works on Linux only.** On macOS it prints `only Linux is supported` and exits; [macOS](#macos) says what still works there.

### Install the collector

- **Release binary.** Each release, from the one that introduced the collector, carries `dot-agent-deck-collector-linux-amd64` and `dot-agent-deck-collector-linux-arm64` beside the deck's own binaries, listed in the same `checksums.txt` and with the same build provenance. Download it the way [Download Binary](installation.md#download-binary) describes for the deck, and save it as `dot-agent-deck-collector`:

  ```bash
  ASSET=dot-agent-deck-collector-linux-amd64   # or dot-agent-deck-collector-linux-arm64
  curl -fsSL -o ~/.local/bin/dot-agent-deck-collector \
    "https://github.com/vfarcic/dot-agent-deck/releases/latest/download/$ASSET"
  chmod +x ~/.local/bin/dot-agent-deck-collector
  ```

- **Nix.** The package installs `dot-agent-deck-collector` next to `dot-agent-deck`.
- **Build from source.** `cargo build --release --locked` builds `target/release/dot-agent-deck-collector` next to the deck; copy it onto your `PATH`.
- **Homebrew** installs only the deck. Download the release binary as above.

`dot-agent-deck upgrade` upgrades the deck, not the collector, so replace the collector the same way you installed it.

### Turn it on

Run the collector as the same user who runs your decks, with the same `OTEL_EXPORTER_OTLP_*` variables the daemon has, so that its metrics land in the same backend as the runs. To see what it would send without sending anything, run it once and read the line of OTLP/JSON it prints:

```bash
dot-agent-deck-collector --once --stdout
```

To keep it running, make it a systemd user service. Put the variables in a file only you can read, `~/.config/dot-agent-deck/collector.env`:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
OTEL_EXPORTER_OTLP_HEADERS=x-honeycomb-team=<your API key>
```

then create `~/.config/systemd/user/dot-agent-deck-collector.service`:

```ini
[Unit]
Description=dot-agent-deck per-run resource collector

[Service]
ExecStart=%h/.local/bin/dot-agent-deck-collector
EnvironmentFile=%h/.config/dot-agent-deck/collector.env
Nice=10
Restart=on-failure

[Install]
WantedBy=default.target
```

and start it:

```bash
chmod 600 ~/.config/dot-agent-deck/collector.env
systemctl --user daemon-reload
systemctl --user enable --now dot-agent-deck-collector
journalctl --user -u dot-agent-deck-collector
```

The log starts with a line naming the endpoint's host it sends to (never your headers). Like the daemon's service, a user service runs while you are logged in, or from boot with lingering on ([Start at boot](daemon-service.md#start-at-boot-linux)). To turn the collector off, `systemctl --user disable --now dot-agent-deck-collector`; the deck is unaffected. When it is stopped, it sends the sample it was taking, if any, and one last sample before it exits, waiting up to 5 seconds for disk measurements already in progress so that the last sample can carry them. A measurement that takes longer, such as the first one of a large worktree, is left out. Sending to your backend counts towards the same limit: the collector exits within 10 seconds of being asked to stop, and what it has not sent by then, the last sample included, is not sent.

### Configuration

| Option | What it does |
| --- | --- |
| `--interval <seconds>` | How often to sample CPU and memory, from 1 to 3600 (default 15). |
| `--disk-interval <seconds>` | How often to measure each worktree a run is working in (default 300). `0` turns disk figures off. |
| `--once` | Take one sample, measure every worktree in use, send it and exit. Exits with status 1 if the send failed or the backend rejected part of it. |
| `--stdout` | Print each sample as one line of OTLP/JSON instead of sending it. Needs no endpoint. |
| `--send-worktree-paths` | Also send each worktree's path as `deck.worktree.path`. Off by default. |
| `--all-users` | Collect the processes of every user it can read, not only its own user's, and label each run with its numeric user id, `deck.run.uid`. Seeing other users' processes needs root; [Whose processes](#whose-processes) says what that sends. |
| `worktree-id <path>` | Print the `deck.worktree.id` of the worktree that holds `<path>`, to tell which worktree an id in your backend is. |

It reads the standard OpenTelemetry variables, with the metrics form of each taking the place of the general one:

| Variable | What it does |
| --- | --- |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, or `OTEL_EXPORTER_OTLP_ENDPOINT` | Where to send. The general one gets `/v1/metrics` added, the metrics one is used as written. |
| `OTEL_EXPORTER_OTLP_METRICS_HEADERS`, or `OTEL_EXPORTER_OTLP_HEADERS` | Headers to send, usually your API key. |
| `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, or `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` (the default) or `http/json`. With `grpc` the collector exits with an error: point it at your backend's HTTP endpoint. |
| `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`, or `OTEL_EXPORTER_OTLP_TIMEOUT` | How long one send may take, retries included, in milliseconds (default 10000). `0`, or anything that is not a number, means the default, as for the deck. |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | `cumulative` (the default) sends each run's CPU as a running total; `delta` or `lowmemory` sends what it used since the previous sample. |
| `OTEL_SERVICE_NAME` | The service name (default `dot-agent-deck-collector`). |
| `OTEL_RESOURCE_ATTRIBUTES` | Your own `key=value` pairs, added to everything it sends. Keys starting with `deck.` are dropped, so a collector started from inside a deck pane does not label every figure with that pane's run. |
| `OTEL_SDK_DISABLED=true`, or `OTEL_METRICS_EXPORTER=none` | The collector sends nothing and exits. |

A send that fails because the backend is busy or cannot be reached (HTTP 429, 502, 503 or 504, no connection, or no answer in time) is tried again, waiting a little longer each time, and at least as long as the backend asks in a `Retry-After` header, while within the timeout above and the sampling interval. A send that still fails, or that the backend turns away with another status, is dropped and logged at most once a minute; the collector carries on. So is a send the backend accepts but whose answer does not arrive whole in that time, or is larger than 64 KiB, because the collector cannot tell whether the backend rejected part of it; the backend may still have kept that sample, so an ended run's final figures can reach it twice. What a dropped sample costs:

- **A run that is still running** loses no CPU with cumulative CPU (the default): its next send that gets through carries its full CPU total, so the dropped sample is a gap in the series, not a wrong total.
- **A run that ended** is sent once with its final figures. If that send is dropped, those figures go again with every later sample until one gets through. Up to 1024 ended runs are kept this way; past that the oldest are dropped, and the log says how many.
- **With `delta` or `lowmemory` temporality**, the CPU a dropped sample carried is lost.
- **Memory, process counts and disk** in a dropped sample are lost; the next sample carries current figures.

When the backend accepts a send but reports that it rejected some of the data points in it, the collector logs how many it rejected, never the backend's own message, and does not send them again. The backend kept the rest of that send, so the ended runs' final figures that went with it are not sent again either. With `--once`, it then exits with status 1.

### Read the figures

Everything the collector sends has `service.name` (`dot-agent-deck-collector`), `service.version`, `host.name`, `deck.collector.schema_version` (`1`, which changes when the set or meaning of these figures changes) and your own `OTEL_RESOURCE_ATTRIBUTES` pairs. Each figure belongs to one of three kinds of resource:

| `deck.attribution` | Which processes | Attributes |
| --- | --- | --- |
| `run` | The processes of one run: the agent and everything it started, including a process that detached from it. | `deck.run.id`, and the run's `deck.pane.id`, `deck.agent`, `deck.role`, `deck.orchestration.name` and `deck.orchestration.id` where it has them, as on its `agent.run` span; `deck.worktree.id` when the run works in a git worktree; with `--all-users`, `deck.run.uid`. |
| `unattributed` | Your other processes: the deck's daemon, your shell, editors, and work started with its environment cleared that left a run's processes. Sent with every sample, even when empty. | none beyond the ones above |
| (none) | A worktree's disk space, one resource per worktree. | `deck.worktree.id` |

| Metric | Unit | What it is |
| --- | --- | --- |
| `deck.run.cpu.time` | seconds | The CPU time the run's processes have used, including processes that have already finished. A running total; its rate is how many cores the run keeps busy. |
| `deck.run.memory.pss` | bytes | The run's proportional memory, as three points with `deck.memory.kind` = `anon`, `file` and `shmem`. Their sum is the run's whole proportional memory. |
| `deck.run.processes` | processes | How many processes the run has right now. |
| `deck.worktree.disk.usage` | bytes | The disk space allocated under the worktree, measured every `--disk-interval`. A measurement that stopped at the collector's limits (below) carries `deck.worktree.disk.complete` = `false`. |

**Proportional memory** splits each page of memory that several processes share among them, so adding it up over a run's processes counts each page once. Resident memory, what most process monitors show, counts a shared page in full for every process that maps it, so a sum of it over a run overstates the run's memory, often two to four times. Of the three kinds, `anon` is memory the run's processes own (heap and stacks) and is the closest figure to what the run costs the machine. `file` is their share of mapped files, mostly program code and shared libraries that every copy of a tool shares and the kernel can drop and reload; `shmem` is shared memory.

**Disk is per worktree, not per run**, because several runs usually work in the same worktree. A run's resource names the worktree it works in, in `deck.worktree.id`: the worktree its agent was in when the collector first saw the run, which stays the run's worktree for the rest of the run. A run that was in no worktree when the collector first saw it has no `deck.worktree.id` for its whole life, even if it moves into a worktree later. The same goes for a run whose worktree the collector could not find within a second of first seeing it, for example in a directory on a network mount that has stopped answering: its CPU and memory are still sent, without a worktree. A directory is a git worktree when it holds a `.git` directory, or the `.git` file a linked worktree or submodule has; a `.git` symlink does not make one. The worktree's disk figure is sent once, however many runs use it, and only while a run is working in it. It counts the files under the worktree's top directory, build output included, each once. It does not follow symlinks, and it skips directories mounted inside the worktree, such as a drive or a bind mount. Where the collector cannot tell whether a directory inside the worktree is mounted there, such as in a container without `/proc` that also refuses the system calls it uses for that, it leaves the directory out and marks the figure incomplete, as described below. A git repository or worktree nested inside it is measured on its own. A main checkout's figure includes its `.git` directory, which holds the history of its linked worktrees too.

**Very large worktrees.** One measurement of a worktree stops once it has taken 10 minutes, looked at 4 million entries, or kept track of a million directories and hard-linked files, whichever comes first, and does not go more than 64 directories deep. The 10 minutes are checked between one look at the filesystem and the next, so a filesystem that stops answering, such as a hung network mount, can hold a measurement longer. When it stops early or leaves something out that way, or cannot read part of the worktree, such as a directory it may list but not look inside, or a directory it cannot tell is mounted there, the figure it sends is what it counted, a lower bound, with `deck.worktree.disk.complete` = `false`, and the log names the worktree's id. A measurement that fails altogether, for example because the worktree was removed, withdraws the worktree's figure: none is sent for it until a measurement succeeds again, and the log names the worktree's id and what went wrong. Because of that attribute, an incomplete figure is a separate series from the worktree's complete figures: when you add up disk use, take one figure per worktree, and do not add an incomplete figure to a complete one of the same worktree. The largest worktree measured while building the collector, 99 GB in 117,000 entries, took 27 seconds.

**`deck.worktree.id` is computed from the worktree's path**, a SHA-256 hash of it: run `dot-agent-deck-collector worktree-id <path>` to see which id a path has. It keeps the path itself out of your backend, but it is not a secret. Anyone who can guess a path can compute its id and recognise it, and the same path on two machines has the same id.

**A run that ended** is sent once more with its final CPU, zero processes and zero memory, then no more.

To see what one run cost, take its `deck.run.id` from its `agent.run` span and filter the collector's metrics by it.

### What it misses, and what it costs

The collector looks at your processes every `--interval` seconds, so it sees what is running at those moments:

- **Work that stays among the run's processes is counted**: commands the agent runs, builds and tests, and their short-lived children. In the workloads measured (a 4-second CPU burst, 300 short commands, and cold `cargo build`s of about 48 seconds), the CPU figures agreed with an exact per-run measurement to within 0.1 s at every interval tried, from 1 to 30 seconds, with one exception below.
- **Sampling can also overcount.** On one of three cold builds at a 1-second interval, the run's CPU came to 48.8 s against 46.9 s measured, 4% high: a process that finished while a sample was being read was counted twice. It did not happen at 2 seconds or slower.
- **Work that detaches from the run and finishes between two samples is undercounted.** A process that double-forks away from the agent keeps counting toward the run while it is alive at a sample, but what it uses after the last sample that saw it is lost. In measurements, a 4-second burst of detached work was counted 77% at a 1-second interval and 22% on average at the default 15 seconds, and many sub-second detached commands about 5%. A shorter `--interval` catches more of it, at a higher cost.
- **The last interval of a run** is lost when its agent exits: up to `--interval` seconds' worth of its CPU.
- **Work started with its environment cleared that also detaches** is counted in the unattributed figures, or, when it finishes between samples, not at all.
- **A process that hides itself from inspection**, as `ssh-agent` and some password managers do, may keep the collector from reading its memory and which run it belongs to. When its memory cannot be read, its CPU still counts but its memory is missing. When the collector cannot tell its run, it keeps a run it already knew the process belonged to; otherwise it uses the run of the process that started it, when it can see that process, or counts it as unattributed.
- **Other users' processes** are not collected unless you pass `--all-users`, and seeing them needs root.

**What it costs.** On a busy machine (16 cores, load average 60 to 85, about 500 processes, 150 to 300 of them the collector's user's, 14 runs), one sample took about 0.2 seconds of CPU, most of it spent reading each process's memory figures: 1.4% of one core at the default 15-second interval, and about 19% at `--interval 1`. Its own memory stayed under 6 MB while sampling. Measuring disk comes on top, every `--disk-interval`, and depends on the size of the worktrees in use: on the same machine, nine worktrees holding 185 GB took 26 seconds the first time and 2.3 seconds once the file system had them cached, with 16 MB of memory. The service above runs it at a lower priority (`Nice=10`), so it gives way to your agents when the machine is busy.

### What it sends

The collector sends numbers, the ids and attributes above, and nothing else. It never sends process names, command lines, process ids, user names, file names, the contents of any environment beyond the run attributes listed above, or paths. With `--all-users` it adds each run's numeric user id. With `--send-worktree-paths` it adds each worktree's path, `deck.worktree.path`, to the worktree's resource and to the resource of each run working in it. Your `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` are sent as you wrote them, as the deck sends them.

### Whose processes

By default the collector looks only at processes of the user it runs as, which is what the service above does. With `--all-users`, run as root, it collects every user's processes: their runs' ids, panes, agents, roles, orchestration names and worktree ids go to your backend too, and so does the work of users who did not set up the collector. Each user's processes are kept in runs of their own, by the user each process runs as, so two users' processes that carry the same `deck.run.id` are two runs, which `deck.run.uid` tells apart.

The collector learns which run a process belongs to from the run attributes the deck puts in the process's environment, and a process can change its own environment. So any process can claim to belong to a run of its user, or to none. A process that starts a program with another run's attributes moves to that run from then on, and the CPU it used before stays with the run it left. The figures tell you where your agents' work went; they are not proof against a process that sets out to be counted somewhere else.

### macOS

macOS is not supported: the collector exits there with `only Linux is supported`. On a Mac, the per-run CPU, memory, process and disk figures are not available. The deck's spans and the agents' own telemetry still work as on Linux.

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
      # A command's first argument: all three carry one.
      - key: deck.operation.word
        action: delete
      - key: deck.operation.commands
        action: delete
      - key: deck.operation.wrapped.word
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

With it off, the deck sends nothing and adds nothing to how it starts agents: no variables, no change to their `PATH`, no Codex or Devin additions, and the Pi extension stays quiet, so each agent's telemetry goes back to whatever you set yourself. An agent that reads the endpoint on its own, such as OpenCode, keeps sending while the endpoint is still set, so unsetting the endpoint is the way to stop everything. Then stop the daemon (`dot-agent-deck daemon stop`) so the next one starts without it.
