# Pi Orchestrator Extension

> **Developer / maintainer reference.** This page documents the internal contract of the bundled Pi extension and is intentionally excluded from the published documentation site.

[Pi](https://github.com/earendil-works/pi) is integrated as a first-class agent (PRD #201). The user-facing setup lives in the published [Orchestration](../orchestration.md) and [Getting Started](../getting-started.md) pages; this page is the contract for maintainers.

The guiding split: **bundle the glue, detect the engine.** Shipping Pi itself would mean shipping a Node runtime and forfeiting the single-static-binary story, so Pi is detected on PATH like `claude`/`opencode`. The only thing compiled into the `dot-agent-deck` binary is the small TypeScript **extension** that gives a Pi pane native tools and event-driven status. Tested against **Pi 0.80.6**; the card-detail events (issue #622) against **Pi 0.87.1**.

## Producer, not a second path

The extension is a higher-fidelity *producer* for the existing protocol — it does **not** invent a parallel status or orchestration channel. It reports into the same `EventType` / `AgentEvent` stream (`src/event.rs`) that the daemon (`src/daemon.rs`) already consumes for every agent. The daemon, TUI, GUI graph, and scheduled runs all see Pi through the identical contract they already use.

## The extension (`pi-extension/`)

A self-contained TypeScript subdirectory with its own JS toolchain: installing its npm dependencies is separate from the Rust build, and cargo builds nothing in it. The fast tier does read its sources: `the_pi_extensions_node_suites_pass` runs its Node suites, and `inbox/pi/006` compares the generated receiver below with its generator and then imports it. Without a usable Node the suites print `SKIP:` and pass, and `inbox/pi/006` skips only the import.

- `src/orchestrator.ts` — **pure logic** (zero imports): argv construction for each command, the Pi-event → report mapping and the tool-detail extraction. This is what the unit tests target, so no running Pi is needed to test it.
- `src/index.ts` — the **Pi-API glue**: the default-export factory `(pi) => void` that registers tools and subscribes to events, wiring the pure functions to Pi.
- `src/inbox-receiver.js` (with `inbox-receiver.d.ts`) — **generated, do not edit**: the deck's inbox receiver, the same source OpenCode's plugins inline (`src/inbox_receiver.js` in the repository root), wrapped as an ES module by `inbox_receiver::pi_module()` (PRD #1666). The fast test `inbox/pi/006` fails when the checked-in copy drifts; `DOT_AGENT_DECK_WRITE_PI_RECEIVER=1 cargo test-fast pi_006` rewrites it. `index.ts` imports it: one receiver per Pi process runs the watcher on the deck's receipt stream.
- `test/orchestrator.test.ts` — unit tests run with `node --import tsx --test` (Node 22; `bun` is not used). Run with `cd pi-extension && npm install && npm test`.

### Native tools

Registered via `pi.registerTool({ name, parameters: Type.Object(...), execute })` (parameters use TypeBox; a tool signals failure by **throwing** from `execute`). Each shells the existing CLI via `pi.exec(cmd, args, { signal })`:

| Tool | Shells |
|---|---|
| `delegate(role, task)` | `dot-agent-deck delegate --to <role> --task <task>` (`--to` repeatable) |
| `work_done(summary, done?)` | `dot-agent-deck work-done --task <summary> [--done]` |

TypeBox and the Pi type definitions resolve from Pi's own runtime (jiti) at load, so they are **not** dependencies of the `pi-extension` package (the only devDependency is `tsx`).

### Event → status mapping

The extension subscribes with `pi.on(name, handler)` (note: `pi.events` is the *inter-extension* bus, not lifecycle) and reports by shelling the CLI seam below. Every report carries `--cwd` from the handler's `ExtensionContext.cwd`, so the card has a directory however it was created (`piEventReport` in `orchestrator.ts`). Lifecycle mapping:

| Pi event | Reported state |
|---|---|
| `session_start` | `finished` (Idle — awaiting first prompt) |
| `agent_start` | `running` |
| `agent_settled` | `finished` (Idle — turn settled) |
| `session_shutdown` | `finished` |
| `agent_end` | *(deliberately unmapped)* — Pi may auto-retry/compact/drain follow-ups after it, so it is not a reliable turn-end signal; `agent_settled` is |
| anything else | *(no `agent-event` emitted)* — so a bogus `--type` can never reach the CLI |

Card-detail mapping (issue #622). Before it the extension reported lifecycle only, so a Pi card had no `Prmt:` row and read `Tools: 0` however many tools Pi ran:

| Pi event | Reported `--type` | Detail |
|---|---|---|
| `before_agent_start` | `prompt` (→ `Thinking` with `user_prompt`) | `--prompt` from the event's `prompt`, cut to `MAX_PROMPT_CHARS` (4000) for argv; nothing is reported for a blank prompt. Fires only for a prompt submitted while Pi is idle |
| `input` with `streamingBehavior` set (issue #1567) | `prompt` | `--prompt` from the event's `text`, cut the same way. Pi sets `streamingBehavior` (`steer` / `followUp`) exactly when it is busy and is queueing the prompt instead of starting a run, which is the case `before_agent_start` never sees. An `input` without it is ignored, so an idle submission is reported once, by `before_agent_start` |
| `tool_execution_start` | `tool-start` (→ `ToolStart`, card `Working`) | `--tool-name`, and `--tool-detail` from `piToolDetail`: `bash` → first command line (120), `read`/`write`/`edit`/`ls` → `path`, `grep`/`find` → `pattern`, anything else → first string argument (80) |
| `tool_execution_end` | `tool-end` (→ `ToolEnd`, completed-tool count +1) | `--tool-name`; a failed call (`isError`) counts too, as a Claude `PostToolUse` does |
| `tool_execution_update` | *(not subscribed)* | |

Every report, with any older-CLI retry, runs through one serial queue (`createSerialQueue`), so the daemon receives reports in the order Pi emitted them: a call's `tool-end` cannot overtake its `tool-start`, nor a slow `agent_start` retry an `agent_settled`. Pi 0.87.1 already awaits each extension handler before emitting its next event (`processEvents` in `pi-agent-core`), and the queue keeps the order from depending on that. It holds only the CLI calls, never a whole handler, so an event Pi emits while another handler is still running (an inbox delivery starts a turn) cannot wait on it. Prompt reports update the card; inbox acknowledgement separately records receipt.

**Parity with the other backends.** A turn ending is `Idle`, not "Needs Input". Claude, OpenCode, and Codex map their turn-end signal (`Stop` / `session.idle`) and session-start to Idle, and surface "Needs Input" (`waiting`) only on a genuine user-blocking signal (a permission prompt / attention notification). Pi's `agent_settled` is its turn-end analog, so it reports `finished` → Idle; because Pi exposes no permission/attention lifecycle event today, it never reports `waiting` — like a Claude agent that never hits a permission prompt. `waiting` stays a valid CLI `--type` (below) so a future Pi user-blocking event can map to it with no wire change.

## Inbox acknowledgement

Pi receives every deck message through its inbox watcher, including seeds and tasks delivered mid-session. Delivery is acknowledged when Pi commits the user message carrying the deck opener, not by prompt-status reports. The `before_agent_start` and busy-time `input` events still supply card prompt details; they do not prove that an inbox message was committed.

The `input` event runs before commitment and another extension can handle or drop it. The extension therefore acknowledges on user `message_start`; `message_end` tracks the turn reply instead. This separates status reporting from receipt. See [Agent messaging](agent-messaging.md#consumers-and-acknowledgement) for all agents' acknowledgement signals.


## The `agent-event` CLI seam

`dot-agent-deck agent-event --type <running|waiting|finished|prompt|tool-start|tool-end> [--reports-prompts] [--cwd D] [--prompt P] [--tool-name N] [--tool-detail T]` (in `src/main.rs`) is the only new CLI surface. It reads `DOT_AGENT_DECK_PANE_ID` (required) and `DOT_AGENT_DECK_AGENT_ID` (optional) from the pane env the daemon already injects, maps the type via `event::agent_event_type_from_state` (`running→Thinking`, `waiting→WaitingForInput`, `finished→Idle`, `prompt→Thinking`, `tool-start→ToolStart`, `tool-end→ToolEnd`, else error), builds a bare `AgentEvent` (agent type `Pi`) with `hook::build_agent_event_cli`, and sends it **raw** via `hook::send_to_socket` — the same path `delegate`/`work-done` use. The daemon's `run_hook_loop` already falls back to `AgentEvent` and `apply_event` drives the card. The builder bounds the detail like the hook builders do: blank values are dropped, the prompt goes through `record_submitted_prompt`, the tool detail keeps its first line cut to 120 bytes and the tool name 80.

**This is zero new wire.** Every type lands on an existing `EventType` variant and every detail on an existing optional `AgentEvent` field (`cwd`, `user_prompt`, `tool_name`, `tool_detail`) with the meaning it already has for the hook-driven agents, so neither `PROTOCOL_VERSION` nor a `.breaking.md` applies. The `--type` vocabulary (`event::AGENT_EVENT_TYPES` / `AGENT_EVENT_TYPES` in `orchestrator.ts`) is the contract the extension's mapping and the docs must agree on. The lifecycle-only invocation (`--type running` and no other flag) is unchanged, so an older extension keeps working against a newer CLI. The reverse — this extension shelling a CLI from before #622 — is reachable: the daemon writes the extension into Pi's directory when it starts, so a newer daemon starting on the same machine hands its extension to the Pi panes an older, still-running daemon spawns, and those name the older binary in `DOT_AGENT_DECK_EXE`. The same happens to a CLI from #622 up to #1567, which knows the detail flags but not `--reports-prompts`. So the extension reports at one of three levels (`REPORT_LEVELS` / `createReporter` in `orchestrator.ts`): `declared` (detail plus `--reports-prompts`, which it puts first among the flags so an older CLI names it as the unknown one), `detail`, and `lifecycle` (the bare `--type <state>` every CLI accepts). It starts at `declared` and steps down one level only when the refusal was clap's own usage error — exit code 2 with stderr opening `error: unexpected argument '--` (`isUnsupportedFlagFailure`, which reads the CLI's exit code and stderr, never the argv) — retrying the same report at the lower level and staying at the level that then got through. A detail report has no lifecycle form and is dropped at that level. Any other failure leaves the level where it is and retries a lifecycle report once, bare, so a transient socket error costs neither the card's status nor the session's detail. A deck whose CLI predates the declaration therefore never gets it, and its Pi panes stay non-confirming, which is what that deck's delivery code expects anyway.

The extension passes each detail as a single `--flag=value` argv element, and the CLI's detail args set `allow_hyphen_values`: both are free text, and a prompt like `--help me` or a command like `-rf build` passed as a separate element would otherwise be parsed as a flag and the whole report refused.

**Turn replies (PRD #1497).** The desktop's reading mode reads a short summary of each turn an agent finishes, from that turn's final reply, and the extension supplies Pi's. It keeps the text of the last assistant message Pi emits (`message_end`, text blocks only; thinking and tool calls are left out, and an errored message with no text gives its `errorMessage`), and every assistant message replaces it, so a final message with nothing to read (thinking or tool calls only, empty, or an error with no `errorMessage`) leaves an empty reply rather than letting earlier commentary stand in for one; a run starting (`agent_start`) forgets it too. A run that settles with nothing to read still reports a reply, an empty one, which the deck says as a turn with no reply to read (audit A2); only a settle with no run or assistant message since the last one reports none. It attaches the reply to that run's `agent_settled` report as `--turn-reply-stdin`, plus `--turn-reply-failed` when the message ended with `stopReason: "error"`, and writes the text to the CLI's stdin (`createTurnReplyTracker` and `piAssistantReply` in `orchestrator.ts`, cut to `MAX_TURN_REPLY_BYTES` of UTF-8). The CLI puts it on the hook-socket line under `turn_reply`, beside the event and never inside it, which is exactly where a Claude-compatible hook's reply goes, so the daemon publishes it to reading subscribers the same way and clamps it to `MAX_TURN_REPLY_BYTES`. The reply flags are the newest the extension sends, so they ride on top of the three levels rather than adding a fourth: a CLI that refuses any flag of a report carrying a reply cannot take the reply, and `createReporter` drops the reply for the rest of the session and sends the same report again at the same level; only a refusal of that steps the level down, exactly as before. Still zero new wire: an older daemon ignores the extra line key (`AgentEvent` does not deny unknown fields), and an older CLI's refusal costs one extra call per session. The reply stays off the command line, where any local user who can read `/proc/<pid>/cmdline` could read it: `pi.exec` spawns with stdin ignored and takes no stdin option (checked on Pi 0.84.4 and 1.1.0), so the report that carries a reply is spawned by the extension itself with `node:child_process` (`execWithStdin`: the same binary, an argv array and no shell, the session's directory, the report's abort signal), and every other report still goes through `pi.exec`. An older CLI exits on its usage error without reading stdin; the write's broken pipe is ignored and the exit code drives the ladder as before. [`turn-replies.md`](turn-replies.md) has the CLI's bounded, time-limited read.

## Native inbox delivery

On `session_start`, the extension starts `dot-agent-deck inbox wait --agent pi --stream`. It receives framed inbox messages and passes each to `pi.sendUserMessage(text, { deliverAs: "followUp" })`. The option starts an idle agent and queues follow-ups on a busy one; it is used for every message, including the first prompt. `sendUserMessage` returns no receipt, so resolution is not an acknowledgement signal.

The stream receives ack controls after committed user messages. If the watcher has gone away, acknowledgement falls back to the CLI. The watcher's stdin answers nothing, and the deck may never hand a committed message over again (it has used its hand-overs, or `max_deliver` is 1), so every committed id stays unconfirmed until `dot-agent-deck ack --json <id>` reports that the deck recorded it: first after one second, or at once when the CLI carries the acknowledgement, then with backoff doubling to 30 seconds, eight asks at most, each id on its own timer so new commits never postpone an older id's next ask. A refusal by id (`undeliverable`, `wrong_agent`, `unknown`) ends it at once; that and running out of asks are shown to the user with Pi's `ui.notify` as a warning naming the message. The daemon records a late acknowledgement of a direct message that ran out of hand-overs (`Inbox::ack`), not of an offer, which has gone back to its pool. A session replacement (`/new`, fork, resume, reload) keeps the ids still unconfirmed and the ids Pi committed, process-wide: the next instance goes on confirming the first at its `session_start`, and acknowledges a redelivery of either instead of handing it to Pi. Pi exiting ends both, as it ends the agent. A failed hand-over remains unacknowledged and can be retried. Session shutdown kills the watcher; /new, fork, switch and reload start a new extension generation. Callbacks check that generation and catch stale-context exceptions. Watchers stop on stdin EOF; Pi stops respawning after stream exit 3 (outside a deck or missing capability).

Each native hand-over opens with an attempt line, `[dot-agent-deck hand-over <instance>.<n>]`, which the extension removes at `input` before the model sees it. That handler consumes a replaced attempt or one whose deck messages already committed. An attempt the handler has not seen can be handed over again after 30 seconds idle with nothing queued (`INBOX_ADMISSION_RECHECK_MS`), or after a later run settles with nothing queued; its late original is consumed at `input`. After `input`, a retry requires an affirmative refusal witness: `before_agent_start` saw the prompt, then `agent_settled` ran with `ctx.signal` set (another run was active), followed by a settle with `ctx.signal` unset and no intervening commit. Without that witness the extension waits for inbox ack-wait redelivery, 60 seconds by default. It replaces an outstanding attempt when Pi has nothing queued and keeps a queued follow-up rather than submitting it again. Failed `isIdle` or pending-message queries do not authorize a retry.

On ack-wait redelivery, Pi answers `{"hold":[id]}` when it holds the id behind an admission or a hand-over may still commit, keeping the message without another submission. When it replaces a lost hand-over after `input` with nothing queued, it sends no `hold`: that hand-over did not reach the agent. See [Long turns: hold and the late ack](agent-messaging.md#long-turns-hold-and-the-late-ack) for the one-hour bound and notice handling.

The witness was checked against Pi 0.84.4, whose extension API gives no refusal result for `sendUserMessage`. It is not an exactly-once execution guarantee: a hand-over still in post-input preflight (auth, compaction, or another extension's later hook) beyond the ack-wait can run twice. An unrelated refusal's settle and the active run's end can also look like the witness while an original attempt remains in a later `before_agent_start` hook. A missed witness falls back to ack-wait redelivery; the attempt guard cannot recall an original already past `input`.

Pi's doorbell `input` handler marks the line handled, fetches without acknowledging and hands the framed content to `sendUserMessage`. Commitment then acknowledges it exactly as on the stream path. This keeps the fallback model from needing to fetch its own message.

The extension no longer calls `get-seed`. Seeds are inbox messages, so a `clear = false` re-delegation uses the same native path as a fresh session. See [Agent messaging](agent-messaging.md) for framing, limits, backoff and doorbell behavior.


## Materialization (`src/orchestrator_ext.rs`)

`index.ts`, `orchestrator.ts` and `inbox-receiver.js` are embedded with `include_str!` pointed at the **real** `pi-extension/src/` files (no fork — editing the extension flows into the binary on rebuild). `materialize(target_dir)` writes them into Pi's subdir discovery layout `<dir>/index.ts` + `<dir>/orchestrator.ts` + `<dir>/inbox-receiver.js`; the real target is `~/.pi/agent/extensions/dot-agent-deck/`. `package.json` is intentionally not embedded — Pi's subdir `index.ts` discovery needs none, and TypeBox resolves from Pi's runtime.

**Auto-materialize at spawn time.** The bundled extension is materialized **automatically** just before a Pi pane is launched — `AgentPtyRegistry::spawn_agent` (`src/agent_pty.rs`) calls `orchestrator_ext::auto_materialize(&opts.env)` when `AgentType::from_command(opts.command) == Some(AgentType::Pi)`, so a user needs no manual step: install `pi` + `command = "pi"` is the whole setup. It writes into the **child's own HOME** (the `HOME`/`PATH` overlay in `opts.env` first, then the process `HOME`), is guarded on `pi` being present, and is idempotent (overwrite, refreshing a stale copy). It is **HOME-unset-safe**: `auto_materialize_core` returns `None` and **skips** when HOME is unset or empty — it never falls back to a `/tmp` guess. `orchestrator setup` (in `src/main.rs` / `orchestrator_ext.rs`) remains the **optional explicit path** — it wires `pi`-on-PATH detection + `default_extension_dir()` to `materialize` — but is no longer required for normal use. That explicit path is **equally HOME-unset-safe**: `default_extension_dir()` returns `None` when HOME is unset or empty (via the strict `home_dir_strict` resolver, no `/tmp`/`./` fallback), and — because it is an explicit user command rather than a best-effort spawn seam — the CLI then **errors non-zero naming HOME** instead of the auto path's silent skip, so it never materializes into a bogus location Pi would never discover.

## No hooks for a Pi pane

A Pi pane installs no Claude Code hook and mutates no `~/.claude/settings.json` — **by construction, with zero gating code.** `hooks_manage::auto_install()` runs only at TUI/dashboard startup, is machine-global, and takes no `AgentType`; the daemon-serve / spawn / scheduler / `agent-event` paths never call it. Design Decision #4 is satisfied without any `AgentType::Pi` branch in `src/hooks_manage.rs`.

## Cross-version contract (rule 12)

Inbox integration ships with `PROTOCOL_VERSION = 11`; older decks and daemons require a coordinated upgrade. The hook-socket inbox verbs carry the recipient generation and capability. Pi's extension no longer relies on the old `GetSeed` request or a typed fallback. See [Agent messaging](agent-messaging.md#contract-and-validation) and [Versioning](versioning.md) for the current boundary.

## Experimental gating

The Pi surface is gated behind `experimental` via the single wrapper `features::show_pi_agent()`, applied only at the render seam in `src/ui.rs` `render_session_card` (an off-flag Pi card falls back to the pre-feature `AgentType::None` placeholder without hiding a running pane). Business logic, the daemon protocol, hooks, the extension, and `agent-event` routing are **not** gated. See [experimental-flag](experimental-flag.md). Graduation issue: `graduate-pi-agent` (`grep show_pi_agent` finds every call site).

## Testing

- Fast tier: the agent-agnostic synthetic harness (`tests/common/synthetic_agent.rs`) exercises delegate/work-done/`agent-event` routing parameterized by agent identity (Pi row instantiated here; the companion cross-agent PRD adds `claude`/`opencode`).
- TS unit tests: `cd pi-extension && npm test`.
- Real-agent tests are lane 2, gated by both `e2e` and `e2e-live`. See `tests/e2e_inbox_pi_live.rs`, `tests/e2e_pi_live.rs` and `tests/e2e_pi_orchestrator.rs`, and find current catalog ids in `tests/CATALOG.md`. Run scoped tests with `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live <filter>` and name the model and results in the report. Credentials belong only on the developer machine, never CI.
