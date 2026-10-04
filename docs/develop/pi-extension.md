# Pi Orchestrator Extension

> **Developer / maintainer reference.** This page documents the internal contract of the bundled Pi extension and is intentionally excluded from the published documentation site.

[Pi](https://github.com/earendil-works/pi) is integrated as a first-class agent (PRD #201). The user-facing setup lives in the published [Orchestration](../orchestration.md) and [Getting Started](../getting-started.md) pages; this page is the contract for maintainers.

The guiding split: **bundle the glue, detect the engine.** Shipping Pi itself would mean shipping a Node runtime and forfeiting the single-static-binary story, so Pi is detected on PATH like `claude`/`opencode`. The only thing compiled into the `dot-agent-deck` binary is the small TypeScript **extension** that gives a Pi pane native tools and event-driven status. Tested against **Pi 0.80.6**; the card-detail events (issue #622) against **Pi 0.87.1**.

## Producer, not a second path

The extension is a higher-fidelity *producer* for the existing protocol — it does **not** invent a parallel status or orchestration channel. It reports into the same `EventType` / `AgentEvent` stream (`src/event.rs`) that the daemon (`src/daemon.rs`) already consumes for every agent. The daemon, TUI, GUI graph, and scheduled runs all see Pi through the identical contract they already use.

## The extension (`pi-extension/`)

A self-contained TypeScript subdirectory; its whole JS toolchain lives there and is kept off the Rust critical path (cargo/nextest never touch it).

- `src/orchestrator.ts` — **pure logic** (zero imports): argv construction for each command, the Pi-event → report mapping and the tool-detail extraction. This is what the unit tests target, so no running Pi is needed to test it.
- `src/index.ts` — the **Pi-API glue**: the default-export factory `(pi) => void` that registers tools and subscribes to events, wiring the pure functions to Pi.
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

Every report, with any older-CLI retry, runs through one serial queue (`createSerialQueue`), so the daemon receives reports in the order Pi emitted them: a call's `tool-end` cannot overtake its `tool-start`, nor a slow `agent_start` retry an `agent_settled`. Pi 0.87.1 already awaits each extension handler before emitting its next event (`processEvents` in `pi-agent-core`), and the queue keeps the order from depending on that. It holds only the CLI calls, never a whole handler, so an event Pi emits while another handler is still running (the seed delivery in `session_start` starts a turn) cannot wait on it. The prompt reports make a Pi pane a prompt-confirming agent; the next section has how and why.

**Parity with the other backends.** A turn ending is `Idle`, not "Needs Input". Claude, OpenCode, and Codex map their turn-end signal (`Stop` / `session.idle`) and session-start to Idle, and surface "Needs Input" (`waiting`) only on a genuine user-blocking signal (a permission prompt / attention notification). Pi's `agent_settled` is its turn-end analog, so it reports `finished` → Idle; because Pi exposes no permission/attention lifecycle event today, it never reports `waiting` — like a Claude agent that never hits a permission prompt. `waiting` stays a valid CLI `--type` (below) so a future Pi user-blocking event can map to it with no wire change.

## Prompt confirmation (issue #1567)

The deck's automatic prompts (a dispatched unit's task, a scheduled job's prompt, a mode seed or orchestrator prompt when the native `get-seed` hand-off did not take it) are typed into the pane and stay **provisional** until the agent reports submitting that prompt (`src/prompt_delivery.rs`). A pane whose agent can report gets the delivery re-submitted when no report comes, and a dispatched unit that never reports gets the "task may never have arrived" notice on its card. A pane whose agent cannot report gets one write and nothing else.

A Pi pane is now the first kind, but only while its extension says so. `prompt_delivery::agent_prompt_reporting(Pi)` is `WhenDeclared`: a Pi producer counts as reporting only when its events carry `prompt_reports_declared = "1"` (`event::PROMPT_REPORTS_DECLARED_METADATA_KEY`), which `agent-event --reports-prompts` stamps and this extension sends on every report. The declaration is sticky per session (`SessionState::prompt_reports_declared`), and a declared inability (`wrapper_prompt_reports_unavailable`) outranks it. The reason for the declaration is the extensions already in the field, none of which reports every prompt:

- before #622 the extension reported no prompt at all;
- from #622 it reported `before_agent_start`, which Pi raises only for a prompt submitted while it is idle. Measured on Pi 0.87.1: a prompt typed while Pi ran a tool was queued as a steering message, submitted and acted on, and never reported — Pi starts no new run for it. Counting such an extension as confirming would re-submit a prompt Pi already took.

A Pi process keeps the extension it loaded at start, so a deck upgraded under a running Pi meets an older one. Without the declaration it stays `CannotReport`, exactly the pre-#1567 behaviour.

What a declared Pi pane changes, path by path:

- **Daemon spawn-time delivery** (`crate::spawn`: `dispatch`, the scheduler, issue-dispatch, and `StartAgent`'s authoring kinds for agents other than a native-seeded Pi): `AgentEvent::reports_submitted_prompt` is true for the pane's frames, so the unconfirmed write is re-submitted under the usual backoff and confirmed by Pi's `prompt` report. A pane the deck spawned as Pi has the #570 standing to accept that producer when it announces itself after the write (`agent_spawned_as_reporting_agent`, which asks only whether the type *can* report).
- **The quiet-unit notice** on a dispatched unit's card follows from the same delivery: a declared Pi unit that never reports its task gets it.
- **TUI-owned deliveries** (an orchestrator role prompt and a mode seed typed in by the TUI): `pane_confirmation_capability` answers `Reports` for the pane, so they arm re-submission too. A Pi start role's prompt is delivered natively and does not go through this.
- **Confirmation latency floor:** Pi stays on `SLOW_CONFIRMATION_LATENCY` (10 s). Its ordinary report is fast — 40 ms from write to confirmation in `scheduler/pi/002` — but a prompt submitted while Pi compacts is held in Pi's own queue and reported when compaction ends (4.4 s measured on a 10k-token context), and the 2 s floor would type a second copy behind it.
- **Not changed:** delegate re-delivery (`delegate_retry`) already covered Pi and takes any turn event as proof; the #666 re-arm needs a `SessionStart`, which Pi never sends.

Residuals, all in the direction of a missed re-submission rather than a duplicate unless noted: a busy-time prompt that another extension's `input` handler swallows after ours has reported it still counts as submitted (an idle one is reported from `before_agent_start`, after every handler and after Pi's model and key checks); if Pi stops being busy between the `input` event and its own check, the prompt is reported twice (once from `input`, once from `before_agent_start`), which is harmless; a compaction that holds a typed prompt for longer than the 10 s floor gets a second copy queued behind it (a duplicate, and the one that remains — a fresh pane, which is where the deck types automatic prompts, has nothing to compact).

## The `agent-event` CLI seam

`dot-agent-deck agent-event --type <running|waiting|finished|prompt|tool-start|tool-end> [--reports-prompts] [--cwd D] [--prompt P] [--tool-name N] [--tool-detail T]` (in `src/main.rs`) is the only new CLI surface. It reads `DOT_AGENT_DECK_PANE_ID` (required) and `DOT_AGENT_DECK_AGENT_ID` (optional) from the pane env the daemon already injects, maps the type via `event::agent_event_type_from_state` (`running→Thinking`, `waiting→WaitingForInput`, `finished→Idle`, `prompt→Thinking`, `tool-start→ToolStart`, `tool-end→ToolEnd`, else error), builds a bare `AgentEvent` (agent type `Pi`) with `hook::build_agent_event_cli`, and sends it **raw** via `hook::send_to_socket` — the same path `delegate`/`work-done` use. The daemon's `run_hook_loop` already falls back to `AgentEvent` and `apply_event` drives the card. The builder bounds the detail like the hook builders do: blank values are dropped, the prompt goes through `record_submitted_prompt`, the tool detail keeps its first line cut to 120 bytes and the tool name 80.

**This is zero new wire.** Every type lands on an existing `EventType` variant and every detail on an existing optional `AgentEvent` field (`cwd`, `user_prompt`, `tool_name`, `tool_detail`) with the meaning it already has for the hook-driven agents, so neither `PROTOCOL_VERSION` nor a `.breaking.md` applies. The `--type` vocabulary (`event::AGENT_EVENT_TYPES` / `AGENT_EVENT_TYPES` in `orchestrator.ts`) is the contract the extension's mapping and the docs must agree on. The lifecycle-only invocation (`--type running` and no other flag) is unchanged, so an older extension keeps working against a newer CLI. The reverse — this extension shelling a CLI from before #622 — is reachable: the daemon writes the extension into Pi's directory when it starts, so a newer daemon starting on the same machine hands its extension to the Pi panes an older, still-running daemon spawns, and those name the older binary in `DOT_AGENT_DECK_EXE`. The same happens to a CLI from #622 up to #1567, which knows the detail flags but not `--reports-prompts`. So the extension reports at one of three levels (`REPORT_LEVELS` / `createReporter` in `orchestrator.ts`): `declared` (detail plus `--reports-prompts`, which it puts first among the flags so an older CLI names it as the unknown one), `detail`, and `lifecycle` (the bare `--type <state>` every CLI accepts). It starts at `declared` and steps down one level only when the refusal was clap's own usage error — exit code 2 with stderr opening `error: unexpected argument '--` (`isUnsupportedFlagFailure`, which reads the CLI's exit code and stderr, never the argv) — retrying the same report at the lower level and staying at the level that then got through. A detail report has no lifecycle form and is dropped at that level. Any other failure leaves the level where it is and retries a lifecycle report once, bare, so a transient socket error costs neither the card's status nor the session's detail. A deck whose CLI predates the declaration therefore never gets it, and its Pi panes stay non-confirming, which is what that deck's delivery code expects anyway.

The extension passes each detail as a single `--flag=value` argv element, and the CLI's detail args set `allow_hyphen_values`: both are free text, and a prompt like `--help me` or a command like `-rf build` passed as a separate element would otherwise be parsed as a flag and the whole report refused.

## Native prompt delivery

A Pi pane receives its first task/seed prompt **natively**, through Pi's own message API, rather than by the daemon typing keystrokes into the PTY. Pi's `session_start` fires before the render-loop injection point, so the seed has to be ready at spawn time and the pane has to pull it. The pieces:

- **`dot-agent-deck get-seed` — read-only verb (`src/main.rs`).** A pane pulls its pending seed by shelling `get-seed`, which sends a `DaemonMessage::GetSeed { pane_id }` request over the **same unversioned hook socket** that `delegate` / `work-done` / `agent-event` use, scoped by `DOT_AGENT_DECK_PANE_ID`, and reads back a single-line `GetSeedResponse` JSON reply. It is read-only — it never mutates daemon state — and prints an empty seed when the daemon has none. The request is tagged `message_type: "get_seed"` (`src/event.rs`) so an **older daemon that does not recognize it fails closed** rather than misinterpreting the frame; `get-seed` then reports an empty seed and the fallback delivers.
- **`StartAgent.seed` — additive protocol field (`src/daemon_protocol.rs`).** A spawn-time seed the daemon stashes for the pane via `AgentPtyRegistry::set_pending_seed`, so it is available *before* pi boots and fires `session_start`. Only a Pi start-role (orchestrator) spawn — and a `clear = true` Pi worker respawn (`src/state.rs`) — carries a seed; every other spawn sends `seed: None`. The field is `skip_serializing_if` empty, so a no-seed `StartAgent` keeps the exact legacy wire shape (see the cross-version note below).
- **Extension delivery (`pi-extension/src/index.ts`).** On `session_start` the extension shells `get-seed`, and if the result is a real (non-blank) seed it calls `pi.sendUserMessage(seed, { deliverAs: "followUp" })` (`SEED_DELIVER_AS` in `orchestrator.ts`). `followUp` on an idle agent both seeds and triggers a turn, so the orchestrator starts working with no keystroke and none of the injection path's timing fragility. Delivery is best-effort — any failure (no binary, no daemon, older daemon) just no-sends and lets the fallback cover it.
- **Bounded exactly-once PTY-injection fallback.** Spawn arms a fallback (`agent_pty::arm_seed_fallback`, window = `seed_fallback_grace()`); the per-agent `seed_delivered_native` arbiter (`src/agent_pty.rs`) records whether the native path fired. Native delivery suppresses injection; if it does not arrive within the grace window the daemon injects the seed into the PTY **exactly once**. This also removes the old pi-worker ~10s `SessionStart` timeout (pi never emits `EventType::SessionStart`).
- **Scope.** Covers the orchestrator seed and `clear = true` worker respawns (a respawn produces a fresh `session_start` for the pull to fire on). A **`clear = false`** re-delegation is mid-session with no respawn, so no native pull is armed and it **keeps the legacy PTY injection** (documented further enhancement). Pi's headless **RPC mode is explicitly rejected** — it has no live interactive session for `sendUserMessage`/`session_start` to work against, so the real-pi e2e reject it.

## Materialization (`src/orchestrator_ext.rs`)

`index.ts` and `orchestrator.ts` are embedded with `include_str!` pointed at the **real** `pi-extension/src/` files (no fork — editing the extension flows into the binary on rebuild). `materialize(target_dir)` writes them into Pi's subdir discovery layout `<dir>/index.ts` + `<dir>/orchestrator.ts`; the real target is `~/.pi/agent/extensions/dot-agent-deck/`. `package.json` is intentionally not embedded — Pi's subdir `index.ts` discovery needs none, and TypeBox resolves from Pi's runtime.

**Auto-materialize at spawn time.** The bundled extension is materialized **automatically** just before a Pi pane is launched — `AgentPtyRegistry::spawn_agent` (`src/agent_pty.rs`) calls `orchestrator_ext::auto_materialize(&opts.env)` when `AgentType::from_command(opts.command) == Some(AgentType::Pi)`, so a user needs no manual step: install `pi` + `command = "pi"` is the whole setup. It writes into the **child's own HOME** (the `HOME`/`PATH` overlay in `opts.env` first, then the process `HOME`), is guarded on `pi` being present, and is idempotent (overwrite, refreshing a stale copy). It is **HOME-unset-safe**: `auto_materialize_core` returns `None` and **skips** when HOME is unset or empty — it never falls back to a `/tmp` guess. `orchestrator setup` (in `src/main.rs` / `orchestrator_ext.rs`) remains the **optional explicit path** — it wires `pi`-on-PATH detection + `default_extension_dir()` to `materialize` — but is no longer required for normal use. That explicit path is **equally HOME-unset-safe**: `default_extension_dir()` returns `None` when HOME is unset or empty (via the strict `home_dir_strict` resolver, no `/tmp`/`./` fallback), and — because it is an explicit user command rather than a best-effort spawn seam — the CLI then **errors non-zero naming HOME** instead of the auto path's silent skip, so it never materializes into a bogus location Pi would never discover.

## No hooks for a Pi pane

A Pi pane installs no Claude Code hook and mutates no `~/.claude/settings.json` — **by construction, with zero gating code.** `hooks_manage::auto_install()` runs only at TUI/dashboard startup, is machine-global, and takes no `AgentType`; the daemon-serve / spawn / scheduler / `agent-event` paths never call it. Design Decision #4 is satisfied without any `AgentType::Pi` branch in `src/hooks_manage.rs`.

## Cross-version contract (rule 12)

Every PRD #201 addition rides existing wire without changing its shape or a field's meaning:

- `agent-event` is an **additive CLI subcommand over the existing `AgentEvent` wire**.
- `get-seed` rides the **unversioned hook socket** (request/response), and is tagged so an older daemon that does not know it fails closed to "no seed" rather than misparsing.
- `StartAgent.seed` is an **additive, `skip_serializing_if`-empty field** — a no-seed `StartAgent` serializes to the exact legacy shape an older daemon parses.

Classification: **no `PROTOCOL_VERSION` bump, no `.breaking.md`** — all additive, all degrade gracefully. See [versioning](versioning.md).

## Experimental gating

The Pi surface is gated behind `experimental` via the single wrapper `features::show_pi_agent()`, applied only at the render seam in `src/ui.rs` `render_session_card` (an off-flag Pi card falls back to the pre-feature `AgentType::None` placeholder without hiding a running pane). Business logic, the daemon protocol, hooks, the extension, and `agent-event` routing are **not** gated. See [experimental-flag](experimental-flag.md). Graduation issue: `graduate-pi-agent` (`grep show_pi_agent` finds every call site).

## Testing

- Fast tier: the agent-agnostic synthetic harness (`tests/common/synthetic_agent.rs`) exercises delegate/work-done/`agent-event` routing parameterized by agent identity (Pi row instantiated here; the companion cross-agent PRD adds `claude`/`opencode`).
- TS unit tests: `cd pi-extension && npm test`.
- Real-agent e2e (`tests/e2e_pi_orchestrator.rs`, `#[cfg(feature = "e2e")]`): a real Pi orchestrator delegates to a real worker and receives `work-done`. Pi authenticates to **Anthropic** (`--provider anthropic --model claude-haiku-4-5`, the cheapest tier in pi's Anthropic catalog) via `ANTHROPIC_API_KEY`, sourced through `vals`/`.env.vals.yaml` (`ref+gcpsecrets://vfarcic/anthropic-api-key`). A plain env var is enough — pi does **not** need its `~/.pi/agent/auth.json` OAuth entry staged into the test HOME, which is what lets the tests hand the child a fresh HOME. The TUI harness `env_clear`s the child, so the test explicitly propagates `ANTHROPIC_API_KEY` + `HOME` to the Pi child. **Temporary:** the tier runs on Anthropic Haiku while the GPT accounts are without credit; the tier is provider-agnostic, so moving it is the `PI_MODEL` constant plus the `--provider` flag and the key name in each of `tests/e2e_pi_orchestrator.rs`, `tests/e2e_pi_worker.rs` and `tests/e2e_pi_live.rs`.
