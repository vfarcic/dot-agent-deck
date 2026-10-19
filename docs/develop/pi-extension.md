# Pi Orchestrator Extension

> **Developer / maintainer reference.** This page documents the internal contract of the bundled Pi extension and is intentionally excluded from the published documentation site.

[Pi](https://github.com/earendil-works/pi) is integrated as a first-class agent (PRD #201). The user-facing setup lives in the published [Orchestration](../orchestration.md) and [Getting Started](../getting-started.md) pages; this page is the contract for maintainers.

The guiding split: **bundle the glue, detect the engine.** Shipping Pi itself would mean shipping a Node runtime and forfeiting the single-static-binary story, so Pi is detected on PATH like `claude`/`opencode`. The only thing compiled into the `dot-agent-deck` binary is the small TypeScript **extension** that gives a Pi pane native tools and event-driven status. Tested against **Pi 0.80.6**; the card-detail events (issue #622) against **Pi 0.87.1**; the inbox receipt path (PRD #1666) was read against **Pi 0.84.4**, the version lane 2 was last run against, and the facts it relies on are listed under [Pi behaviour the receipt path relies on](#pi-behaviour-the-receipt-path-relies-on).

## Producer, not a second path

The extension is a higher-fidelity *producer* for the existing protocol — it does **not** invent a parallel status or orchestration channel. It reports into the same `EventType` / `AgentEvent` stream (`src/event.rs`) that the daemon (`src/daemon.rs`) already consumes for every agent. The daemon, TUI, GUI graph, and scheduled runs all see Pi through the identical contract they already use.

## The extension (`pi-extension/`)

A self-contained TypeScript subdirectory with its own JS toolchain: installing its npm dependencies is separate from the Rust build, and cargo builds nothing in it. The fast tier does read its sources: `the_pi_extensions_node_suites_pass` runs its Node suites, and `inbox/pi/006` compares the generated receiver below with its generator and then imports it. Without a usable Node the suites print `SKIP:` and pass, and `inbox/pi/006` skips only the import.

- `src/orchestrator.ts` — **pure logic** (zero imports): argv construction for each command, the Pi-event → report mapping and the tool-detail extraction. This is what the unit tests target, so no running Pi is needed to test it.
- `src/index.ts` — the **Pi-API glue**: the default-export factory `(pi) => void` that registers tools and subscribes to events, wiring the pure functions to Pi. It also holds the inbox glue around the generated receiver: the process state, the hand-over, the `input` gate, the doorbell, commit attribution and the settle evidence.
- `src/inbox-receiver.js` (with `inbox-receiver.d.ts`) — **generated, do not edit**: the deck's inbox receiver, the same source OpenCode's plugins inline (`src/inbox_receiver.js` in the repository root), wrapped as an ES module by `inbox_receiver::pi_module()` (PRD #1666). The fast test `inbox/pi/006` fails when the checked-in copy drifts; `DOT_AGENT_DECK_WRITE_PI_RECEIVER=1 cargo test-fast pi_006` rewrites it. `index.ts` imports it: one receiver per Pi process runs the watcher on the deck's receipt stream.
- `test/orchestrator.test.ts`, `test/inbox.test.ts` and `test/inbox-receiver.test.ts` — unit tests run with `node --import tsx --test` (`bun` is not used). Run with `cd pi-extension && npm install && npm test`. `inbox.test.ts` drives `index.ts` against a stand-in Pi and a fake watcher speaking the receipt protocol; each Node-backed `inbox/pi/0NN` pin is a `#[spec]` wrapper in `src/orchestrator_ext.rs` that runs the tests whose names start with that pin (the other pins are Rust or e2e tests). Without Node 23.6 or newer the wrappers print `SKIP:` and pass, so a green claim names the Node version and shows no `SKIP:`; off Unix a test that skips itself is reported as a skip (`node_pin_verdict`).

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

## Inbox receipts

Pi is on the deck's receipt policy (PRD #1666): the extension reports where each deck message stands — held, handed, committed, not handed or lost — and the daemon keeps a receipt of each, as it does for OpenCode. A message is committed when Pi starts the user message carrying the deck's opener (`message_start`), never at the pre-commit `input` event, which another extension can still handle or drop; `message_end` tracks the turn reply instead. The `before_agent_start` and busy-time `input` events still supply card prompt details, and are no proof of receipt. The protocol and the daemon's side are in [Agent messaging](agent-messaging.md#receipts-and-possession-opencode-and-pi); this section is what the extension does.

## The `agent-event` CLI seam

`dot-agent-deck agent-event --type <running|waiting|finished|prompt|tool-start|tool-end> [--reports-prompts] [--cwd D] [--prompt P] [--tool-name N] [--tool-detail T]` (in `src/main.rs`) is the only new CLI surface. It reads `DOT_AGENT_DECK_PANE_ID` (required) and `DOT_AGENT_DECK_AGENT_ID` (optional) from the pane env the daemon already injects, maps the type via `event::agent_event_type_from_state` (`running→Thinking`, `waiting→WaitingForInput`, `finished→Idle`, `prompt→Thinking`, `tool-start→ToolStart`, `tool-end→ToolEnd`, else error), builds a bare `AgentEvent` (agent type `Pi`) with `hook::build_agent_event_cli`, and sends it **raw** via `hook::send_to_socket` — the same path `delegate`/`work-done` use. The daemon's `run_hook_loop` already falls back to `AgentEvent` and `apply_event` drives the card. The builder bounds the detail like the hook builders do: blank values are dropped, the prompt goes through `record_submitted_prompt`, the tool detail keeps its first line cut to 120 bytes and the tool name 80.

**This is zero new wire.** Every type lands on an existing `EventType` variant and every detail on an existing optional `AgentEvent` field (`cwd`, `user_prompt`, `tool_name`, `tool_detail`) with the meaning it already has for the hook-driven agents, so neither `PROTOCOL_VERSION` nor a `.breaking.md` applies. The `--type` vocabulary (`event::AGENT_EVENT_TYPES` / `AGENT_EVENT_TYPES` in `orchestrator.ts`) is the contract the extension's mapping and the docs must agree on. The lifecycle-only invocation (`--type running` and no other flag) is unchanged, so an older extension keeps working against a newer CLI. The reverse — this extension shelling a CLI from before #622 — is reachable: the daemon writes the extension into Pi's directory when it starts, so a newer daemon starting on the same machine hands its extension to the Pi panes an older, still-running daemon spawns, and those name the older binary in `DOT_AGENT_DECK_EXE`. The same happens to a CLI from #622 up to #1567, which knows the detail flags but not `--reports-prompts`. So the extension reports at one of three levels (`REPORT_LEVELS` / `createReporter` in `orchestrator.ts`): `declared` (detail plus `--reports-prompts`, which it puts first among the flags so an older CLI names it as the unknown one), `detail`, and `lifecycle` (the bare `--type <state>` every CLI accepts). It starts at `declared` and steps down one level only when the refusal was clap's own usage error — exit code 2 with stderr opening `error: unexpected argument '--` (`isUnsupportedFlagFailure`, which reads the CLI's exit code and stderr, never the argv) — retrying the same report at the lower level and staying at the level that then got through. A detail report has no lifecycle form and is dropped at that level. Any other failure leaves the level where it is and retries a lifecycle report once, bare, so a transient socket error costs neither the card's status nor the session's detail. A deck whose CLI predates the declaration therefore never gets it, and its Pi panes stay non-confirming, which is what that deck's delivery code expects anyway.

The extension passes each detail as a single `--flag=value` argv element, and the CLI's detail args set `allow_hyphen_values`: both are free text, and a prompt like `--help me` or a command like `-rf build` passed as a separate element would otherwise be parsed as a flag and the whole report refused.

**Turn replies (PRD #1497).** The desktop's reading mode reads a short summary of each turn an agent finishes, from that turn's final reply, and the extension supplies Pi's. It keeps the text of the last assistant message Pi emits (`message_end`, text blocks only; thinking and tool calls are left out, and an errored message with no text gives its `errorMessage`), and every assistant message replaces it, so a final message with nothing to read (thinking or tool calls only, empty, or an error with no `errorMessage`) leaves an empty reply rather than letting earlier commentary stand in for one; a run starting (`agent_start`) forgets it too. A run that settles with nothing to read still reports a reply, an empty one, which the deck says as a turn with no reply to read (audit A2); only a settle with no run or assistant message since the last one reports none. It attaches the reply to that run's `agent_settled` report as `--turn-reply-stdin`, plus `--turn-reply-failed` when the message ended with `stopReason: "error"`, and writes the text to the CLI's stdin (`createTurnReplyTracker` and `piAssistantReply` in `orchestrator.ts`, cut to `MAX_TURN_REPLY_BYTES` of UTF-8). The CLI puts it on the hook-socket line under `turn_reply`, beside the event and never inside it, which is exactly where a Claude-compatible hook's reply goes, so the daemon publishes it to reading subscribers the same way and clamps it to `MAX_TURN_REPLY_BYTES`. The reply flags are the newest the extension sends, so they ride on top of the three levels rather than adding a fourth: a CLI that refuses any flag of a report carrying a reply cannot take the reply, and `createReporter` drops the reply for the rest of the session and sends the same report again at the same level; only a refusal of that steps the level down, exactly as before. Still zero new wire: an older daemon ignores the extra line key (`AgentEvent` does not deny unknown fields), and an older CLI's refusal costs one extra call per session. The reply stays off the command line, where any local user who can read `/proc/<pid>/cmdline` could read it: `pi.exec` spawns with stdin ignored and takes no stdin option (checked on Pi 0.84.4 and 1.1.0), so the report that carries a reply is spawned by the extension itself with `node:child_process` (`execWithStdin`: the same binary, an argv array and no shell, the session's directory, the report's abort signal), and every other report still goes through `pi.exec`. An older CLI exits on its usage error without reading stdin; the write's broken pipe is ignored and the exit code drives the ladder as before. [`turn-replies.md`](turn-replies.md) has the CLI's bounded, time-limited read.

## Native inbox delivery

The extension runs the deck's generated receiver (`src/inbox-receiver.js`), one per Pi process, kept on `globalThis` so every session instance shares it: `/new`, resume, fork and reload keep the same watcher, and a reload of a different `INBOX_VERSION` replaces the receiver after draining the old watcher. The lifecycle contract — session tokens, attempts held while no session is current, what each session replacement releases, commit witnesses, status-report order, the version replacement — is in [Pi: one receiver per process](agent-messaging.md#pi-one-receiver-per-process). Where it lives in `index.ts`:

| Pi event | Handler | What it does for the inbox |
|---|---|---|
| `session_start` | `sessionStart` | takes the next session token, becomes current, takes over a reloaded instance's passed attempts, `receiver.ensure()`, hands over what waited in `pending` |
| `session_shutdown` | `sessionShutdown` | for any reason but `reload` (`new`, `resume`, `fork`): releases its passed, uncommitted attempts `lost` and returns its sent-unseen attempts to the front of `pending`; for `reload`: releases nothing. Never touches the watcher |
| (receiver hands out a message) | `deliver` → `pump` → `handOver` | one attempt at a time from `pending`, in order, each once Pi decided the one before it (`admitting`: it started, was seen queued, was released or refused, or stalled idle for `INBOX_UNSEEN_RECHECK_MS`; an earlier attempt of the same message holds back nothing): `may`, then with no await the token, epoch, disposal and `receiver.ready` checks, then `sendUserMessage(tagHandOver(tag, text), { deliverAs: "followUp" })`. A submission no gate saw is submitted again to a `/new`, resume or fork successor; the tag check lets one pass a gate |
| `input` from an extension, with our tag | `gate` | the hand-over point: `transform` (tag stripped) and `handed`, or consumed (`handled`) |
| `input` that is only the doorbell line | `doorbell` | fetches to this receiver with `--adapter-integration 2 --rx`, then transforms in place or consumes |
| `message_start` (user) | `commit` | commits every passed, or sent, attempt of the deck ids it carries |
| `before_agent_start` | `starting` | marks the attempts the prompt carries as started, for the settle evidence |
| `agent_start` | | counts a run (`runs`), which dates hand-overs for the settle evidence |
| `agent_settled` | `settled` | releases `lost` only on positive settle evidence; a sent attempt the gate never saw becomes `not_handed` |

The gate and the doorbell each re-check, synchronously before they return, that the content may still pass: that is the fence described in [Agent messaging](agent-messaging.md#pi-one-receiver-per-process). The gate never returns undefined for our tag, which Pi would pass on to the model; everything it does not pass, it consumes. The receiver is handed only an absolute deck binary (`isAbsoluteOverride`), so `resolveDeckBin`'s bare-name fallback for an older deck never starts a watcher.

What the extension no longer does: it sends no `hold`, `nack` or stdin acknowledgement, does not confirm acknowledgements with `dot-agent-deck ack --json`, does not re-send a hand-over that passed a gate (only the daemon repeats one, marked, through a new attempt; a submission no gate saw before a `/new`, resume or fork is submitted again to the successor), and does not call `get-seed`, which the daemon refuses for a Pi generation. A seed is the pane's first inbox message and arrives like any other.

### Pi behaviour the receipt path relies on

Read in the installed Pi 0.84.4 (`@earendil-works/pi-coding-agent`, `dist/core/`); re-check them when lane 2's Pi changes:

- `sendUserMessage` returns nothing and routes asynchronous errors to the runner, where extensions cannot see them; only a synchronous throw (a stale `pi`) is visible. Pi can run `input` handlers before `sendUserMessage` returns, so the attempt is recorded before the call.
- `prompt()` runs the `input` handlers, then its provider-auth check, then `before_agent_start`, then the agent run. A provider with no configured auth throws after our gate and before any start report, and emits no settle: the case not-started attention exists for (inbox/pi/022).
- `/new`, resume and fork tear the session down (`abort()`, `session_shutdown`, `dispose()`) and start a new runtime with an empty follow-up queue; a reload keeps the same `AgentSession` and its queue. Restoring a session emits no `message_start`.
- The agent runs one prompt at a time. A prompt that leaves its preflight while another run is active is refused ("Agent is already processing a prompt", reported to the runner), and the refusal's `agent_settled` clears the session's running flag while the other run goes on: `isIdle()` then reads true, so a later `sendUserMessage` takes the preflight as well and is refused in turn. Two of our hand-overs in that preflight together therefore cost a delivery per extra message, which is why `pump` waits for each hand-over to be decided (inbox/pi/036, inbox/load/003).
- `ctx.signal` is set only while a run is active; `isIdle()` and `hasPendingMessages()` answer the settle evidence, and a call that throws counts as "do not release".

## Materialization (`src/orchestrator_ext.rs`)

`index.ts`, `orchestrator.ts` and `inbox-receiver.js` are embedded with `include_str!` pointed at the **real** `pi-extension/src/` files (no fork — editing the extension flows into the binary on rebuild). `materialize(target_dir)` writes them into Pi's subdir discovery layout `<dir>/index.ts` + `<dir>/orchestrator.ts` + `<dir>/inbox-receiver.js`; the real target is `~/.pi/agent/extensions/dot-agent-deck/`. `package.json` is intentionally not embedded — Pi's subdir `index.ts` discovery needs none, and TypeBox resolves from Pi's runtime.

**When the deck writes it.** The daemon materializes the extension once when it starts serving (`auto_materialize`, called from the `daemon serve` entry), into the directory Pi resolves from the daemon's environment (`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`, plus `extensions/dot-agent-deck`), guarded on `pi` being present and skipping when neither `PI_CODING_AGENT_DIR` nor `HOME` is set, never guessing `/tmp`. That covers every launch command, a launcher included. It writes it again for each spawn detected as Pi (`pi_spawn`: the declared agent type, else the basename of the command's first word), before the child starts, into the directory the **child** resolves from its env overlay, a relative one in its working directory (`refresh_for_spawn`, PRD #1666 G6), so a Pi whose own `HOME` or `PI_CODING_AGENT_DIR` points elsewhere, or whose copy another deck overwrote, loads this deck's receiver. A failed spawn refresh shows `refresh_failed_pi` on both clients' cards and names the directory in the daemon's warning. It never writes for an unrelated agent start (#210). Each file is published by an atomic rename of an owner-only temp file, and a destination that is a symlink is refused. `orchestrator setup` (in `src/main.rs` / `orchestrator_ext.rs`) remains the explicit path: it wires `pi`-on-PATH detection and `default_extension_dir()` to `materialize`, and because it is an explicit command it exits non-zero naming `HOME` when `HOME` is unset or empty, rather than skipping.

## No hooks for a Pi pane

A Pi pane installs no Claude Code hook and mutates no `~/.claude/settings.json` — **by construction, with zero gating code.** `hooks_manage::auto_install()` runs only at TUI/dashboard startup, is machine-global, and takes no `AgentType`; the daemon-serve / spawn / scheduler / `agent-event` paths never call it. Design Decision #4 is satisfied without any `AgentType::Pi` branch in `src/hooks_manage.rs`.

## Cross-version contract (rule 12)

Inbox integration ships with `PROTOCOL_VERSION = 11`; older decks and daemons require a coordinated upgrade. The hook-socket inbox verbs carry the recipient generation and capability. Pi's extension no longer relies on the old `GetSeed` request or a typed fallback. An extension from before the receipt stream is refused by a daemon that has the Pi slice (its stream and its doorbell fetch without `--rx` answer `adapter_too_old`, its `get-seed` no seed), and the card says to restart the agent; a restart refreshes the extension when the deck identifies the agent as Pi. See [Agent messaging](agent-messaging.md#contract-and-validation) and [Versioning](versioning.md) for the current boundary.

## Experimental gating

The Pi surface is gated behind `experimental` via the single wrapper `features::show_pi_agent()`, applied only at the render seam in `src/ui.rs` `render_session_card` (an off-flag Pi card falls back to the pre-feature `AgentType::None` placeholder without hiding a running pane). Business logic, the daemon protocol, hooks, the extension, and `agent-event` routing are **not** gated. See [experimental-flag](experimental-flag.md). Graduation issue: `graduate-pi-agent` (`grep show_pi_agent` finds every call site).

## Testing

- Fast tier: the agent-agnostic synthetic harness (`tests/common/synthetic_agent.rs`) exercises delegate/work-done/`agent-event` routing parameterized by agent identity (Pi row instantiated here; the companion cross-agent PRD adds `claude`/`opencode`).
- TS unit tests: `cd pi-extension && npm test`.
- Real-agent tests are lane 2, gated by both `e2e` and `e2e-live`. See `tests/e2e_inbox_pi_live.rs`, `tests/e2e_pi_live.rs` and `tests/e2e_pi_orchestrator.rs`, and find current catalog ids in `tests/CATALOG.md`. Run scoped tests with `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live <filter>` and name the model and results in the report. Credentials belong only on the developer machine, never CI.
