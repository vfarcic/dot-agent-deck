# Gate telemetry: spans from `affected-checks --run`

`cargo xtask affected-checks --run` (issue #1850) sends an OpenTelemetry trace for each run: one `affected_checks.run` span, and one `affected_checks.command` child per command it ran, saying where each command ran and how long it took. It exists to compare gate time with and without a remote runner, for example a batch of dispatched units with `DAD_REMOTE_EXEC` unset against a batch with it set to `rwx` ([rwx.md](rwx.md#sandboxed-runs-for-agents)). It is repository tooling: the deck, the TUI and the desktop app know nothing about it, and nothing a user of the app sees changes.

The code is `xtask/linkage-check/src/gate_telemetry.rs`; `affected_checks.rs` records the spans.

## Turning it on

It reads the standard OpenTelemetry variables, the same ones the deck's own export reads ([`docs/telemetry.md`](../telemetry.md#turn-it-on)):

- **On** when `OTEL_EXPORTER_OTLP_ENDPOINT` (sent to `<endpoint>/v1/traces`) or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (used as written) is set.
- **Off** when neither is set, when `OTEL_SDK_DISABLED=true`, when `OTEL_TRACES_EXPORTER` is set and names no `otlp`, or when the protocol is `grpc` or unrecognised. Off means no exporter is built and no connection is made.
- `OTEL_EXPORTER_OTLP_HEADERS` (or `_TRACES_HEADERS`, which replaces it) carries the backend's credentials, a header named twice keeping its last value whatever the case; `OTEL_EXPORTER_OTLP_PROTOCOL` (or `_TRACES_PROTOCOL`) picks `http/protobuf`, the default, or `http/json`.
- `OTEL_RESOURCE_ATTRIBUTES` becomes the resource, and `OTEL_SERVICE_NAME` (or a `service.name` pair in it) the service name, `dot-agent-deck` when neither is set so the spans land beside the deck's own.

An agent under a deck with telemetry on inherits the daemon's endpoint and headers, and the deck adds `deck.run.id`, `deck.pane.id`, `host.name` (and `deck.role` for an orchestration role) to its `OTEL_RESOURCE_ATTRIBUTES` (`src/telemetry/propagation.rs`). That list does not always reach the gate run: Claude Code removes every `OTEL_*` variable from the commands it runs (issue #1893). So when the resource has no `deck.run.id` it is taken from `DOT_AGENT_DECK_TELEMETRY_RUN_ID` (the deck sets it equal to the run's `deck.run.id` when it puts its telemetry shim on the agent's `PATH`), a missing `deck.pane.id` from `DOT_AGENT_DECK_PANE_ID`, and a missing `host.name` from the machine's host name, read as the deck reads its own (`gethostname(2)`, `COMPUTERNAME` off Unix). A pair in `OTEL_RESOURCE_ATTRIBUTES` wins over its fallback unless its value is blank. A gate run an agent starts therefore joins that agent's run with nothing else to set: query `affected_checks.run` by `deck.run.id`. The endpoint still has to reach the gate run; one Claude Code strips and nothing puts back leaves telemetry off.

The test aliases set `OTEL_SDK_DISABLED=true` for the tests only (`.cargo/config.toml`), which is why it must not move into that file's `[env]`: that would switch these spans off too.

## It never fails or stalls the gates

The spans are sent once, after the last command and after the sandbox is stopped, in one request on a separate thread. The helper waits for that request at most 3 seconds (less when `OTEL_EXPORTER_OTLP_TIMEOUT` is lower) and then exits with the gates' own status whatever happened: an unreachable collector, an HTTP error or a timeout is dropped without output, and a panic in the exporter changes nothing but the stderr line Rust prints for any panicking thread. The module writes no output of its own, so with telemetry off the helper prints exactly what it printed before. A run interrupted before it finishes (Ctrl+C, a kill) sends nothing.

## The spans

`affected_checks.run`, one per `--run`, from the start of the invocation (so it includes computing the change and probing the runner) to the end of the sandbox stop. A `--run` that cannot compute the change still sends one, `failed` with plan `unknown` and no command span, without probing the runner:

| Attribute | Value |
| --- | --- |
| `gates.runner.requested` | What `DAD_REMOTE_EXEC` asked for, trimmed: `rwx`, or whatever else it says. `local` when it is unset or empty. |
| `gates.runner.used` | `local`, or the runner's name (`rwx`). |
| `gates.runner.fallback_reason` | Only when a requested runner was not used: `cli_missing`, `not_logged_in`, `config_missing` or `unknown_runner`, several joined by `,` (for example `cli_missing,config_missing`). |
| `gates.plan` | `full` (any path needs the full gates), `text` (every path is mapped text), `none` (no change, so no command) or `unknown` (the change could not be computed). |
| `gates.commands.count` | How many commands the plan has. |
| `gates.outcome` | `passed`, or `failed` when a command failed or could not start, or the change could not be computed. |
| `gates.failed_command` | The first command that failed, as planned. Only on a failed run. |

`affected_checks.command`, a child of the run per command that ran (none after the first failure, because the helper stops there):

| Attribute | Value |
| --- | --- |
| `gates.command` | The command as planned, without the runner's prefix: `cargo test-fast`, not `rwx sandbox exec -- cargo test-fast`. |
| `gates.command.runner` | `local` or the runner's name. With `rwx`, `cargo fmt` stays `local`. |
| `gates.command.outcome` | `passed` or `failed`. |
| `gates.command.exit_code` | The exit code, when the command exited with one; absent when it was killed by a signal or could not start. |

A failed span also carries the OTLP error status. The span's duration is the command's wall time, which for a remote command includes syncing the worktree to the sandbox. A new runner adds a value to `gates.runner.used` and `gates.command.runner`, not a field.

## Why it encodes OTLP by hand

The workspace has no OpenTelemetry crate: the deck's own export (PRD #1741, `src/telemetry/otlp.rs`) writes OTLP by hand over `reqwest`. This does the same for the handful of fields it fills, over the same `reqwest` with its `blocking` feature, so `Cargo.lock` gained no crate.
