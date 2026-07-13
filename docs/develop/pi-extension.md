# Pi Orchestrator Extension

> **Developer / maintainer reference.** This page documents the internal contract of the bundled Pi extension and is intentionally excluded from the published documentation site.

[Pi](https://github.com/earendil-works/pi) is integrated as a first-class agent (PRD #201). The user-facing setup lives in the published [Orchestration](../orchestration.md) page (the "Using Pi as an agent" section); this page is the contract for maintainers.

The guiding split: **bundle the glue, detect the engine.** Shipping Pi itself would mean shipping a Node runtime and forfeiting the single-static-binary story, so Pi is detected on PATH like `claude`/`opencode`. The only thing compiled into the `dot-agent-deck` binary is the small TypeScript **extension** that gives a Pi pane native tools and event-driven status. Tested against **Pi 0.80.6**.

## Producer, not a second path

The extension is a higher-fidelity *producer* for the existing protocol — it does **not** invent a parallel status or orchestration channel. It reports into the same `EventType` / `AgentEvent` stream (`src/event.rs`) that the daemon (`src/daemon.rs`) already consumes for every agent. The daemon, TUI, GUI graph, and scheduled runs all see Pi through the identical contract they already use.

## The extension (`pi-extension/`)

A self-contained TypeScript subdirectory; its whole JS toolchain lives there and is kept off the Rust critical path (cargo/nextest never touch it).

- `src/orchestrator.ts` — **pure logic** (zero imports): argv construction for each command and the Pi-event → state-string mapping. This is what the unit tests target, so no running Pi is needed to test it.
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

The extension subscribes with `pi.on(name, handler)` (note: `pi.events` is the *inter-extension* bus, not lifecycle) and reports status by shelling the CLI seam below. Mapping:

| Pi event | Reported state |
|---|---|
| `session_start` | `waiting` |
| `agent_start` | `running` |
| `agent_settled` | `waiting` |
| `session_shutdown` | `finished` |
| `agent_end` | *(deliberately unmapped)* — Pi may auto-retry/compact/drain follow-ups after it, so it is not a reliable idle signal; `agent_settled` is |
| anything else | *(no `agent-event` emitted)* — so a bogus `--type` can never reach the CLI |

## The `agent-event` CLI seam

`dot-agent-deck agent-event --type <running|waiting|finished>` (in `src/main.rs`) is the only new CLI surface. It reads `DOT_AGENT_DECK_PANE_ID` (required) and `DOT_AGENT_DECK_AGENT_ID` (optional) from the pane env the daemon already injects, maps the state via `event::agent_event_type_from_state` (`running→Thinking`, `waiting→WaitingForInput`, `finished→Idle`, else error), builds a bare `AgentEvent` (agent type `Pi`), and sends it **raw** via `hook::send_to_socket` — the same path `delegate`/`work-done` use. The daemon's `run_hook_loop` already falls back to `AgentEvent` and `apply_event` drives the status.

**This is zero new wire.** The `--type` vocabulary (`running`/`waiting`/`finished`) is the contract the extension's mapping and the docs must agree on.

## Native prompt delivery

A Pi pane receives its first task/seed prompt **natively**, through Pi's own message API, rather than by the daemon typing keystrokes into the PTY. Pi's `session_start` fires before the render-loop injection point, so the seed has to be ready at spawn time and the pane has to pull it. The pieces:

- **`dot-agent-deck get-seed` — read-only verb (`src/main.rs`).** A pane pulls its pending seed by shelling `get-seed`, which sends a `DaemonMessage::GetSeed { pane_id }` request over the **same unversioned hook socket** that `delegate` / `work-done` / `agent-event` use, scoped by `DOT_AGENT_DECK_PANE_ID`, and reads back a single-line `GetSeedResponse` JSON reply. It is read-only — it never mutates daemon state — and prints an empty seed when the daemon has none. The request is tagged `message_type: "get_seed"` (`src/event.rs`) so an **older daemon that does not recognize it fails closed** rather than misinterpreting the frame; `get-seed` then reports an empty seed and the fallback delivers.
- **`StartAgent.seed` — additive protocol field (`src/daemon_protocol.rs`).** A spawn-time seed the daemon stashes for the pane via `AgentPtyRegistry::set_pending_seed`, so it is available *before* pi boots and fires `session_start`. Only a Pi start-role (orchestrator) spawn — and a `clear = true` Pi worker respawn (`src/state.rs`) — carries a seed; every other spawn sends `seed: None`. The field is `skip_serializing_if` empty, so a no-seed `StartAgent` keeps the exact legacy wire shape (see the cross-version note below).
- **Extension delivery (`pi-extension/src/index.ts`).** On `session_start` the extension shells `get-seed`, and if the result is a real (non-blank) seed it calls `pi.sendUserMessage(seed, { deliverAs: "followUp" })` (`SEED_DELIVER_AS` in `orchestrator.ts`). `followUp` on an idle agent both seeds and triggers a turn, so the orchestrator starts working with no keystroke and none of the injection path's timing fragility. Delivery is best-effort — any failure (no binary, no daemon, older daemon) just no-sends and lets the fallback cover it.
- **Bounded exactly-once PTY-injection fallback.** Spawn arms a fallback (`agent_pty::arm_seed_fallback`, window = `seed_fallback_grace()`); the per-agent `seed_delivered_native` arbiter (`src/agent_pty.rs`) records whether the native path fired. Native delivery suppresses injection; if it does not arrive within the grace window the daemon injects the seed into the PTY **exactly once**. This also removes the old pi-worker ~10s `SessionStart` timeout (pi never emits `EventType::SessionStart`).
- **Scope.** Covers the orchestrator seed and `clear = true` worker respawns (a respawn produces a fresh `session_start` for the pull to fire on). A **`clear = false`** re-delegation is mid-session with no respawn, so no native pull is armed and it **keeps the legacy PTY injection** (documented further enhancement). Pi's headless **RPC mode is explicitly rejected** — it has no live interactive session for `sendUserMessage`/`session_start` to work against, so the real-pi e2e reject it.

## Materialization (`src/orchestrator_ext.rs`)

`index.ts` and `orchestrator.ts` are embedded with `include_str!` pointed at the **real** `pi-extension/src/` files (no fork — editing the extension flows into the binary on rebuild). `materialize(target_dir)` writes them into Pi's subdir discovery layout `<dir>/index.ts` + `<dir>/orchestrator.ts`; the real target is `~/.pi/agent/extensions/dot-agent-deck/`. `package.json` is intentionally not embedded — Pi's subdir `index.ts` discovery needs none, and TypeBox resolves from Pi's runtime.

**Auto-materialize at spawn time.** The bundled extension is materialized **automatically** just before a Pi pane is launched — `AgentPtyRegistry::spawn_agent` (`src/agent_pty.rs`) calls `orchestrator_ext::auto_materialize(&opts.env)` when `AgentType::from_command(opts.command) == Some(AgentType::Pi)`, so a user needs no manual step: install `pi` + `command = "pi"` is the whole setup. It writes into the **child's own HOME** (the `HOME`/`PATH` overlay in `opts.env` first, then the process `HOME`), is guarded on `pi` being present, and is idempotent (overwrite, refreshing a stale copy). It is **HOME-unset-safe**: `auto_materialize_core` returns `None` and **skips** when HOME is unset or empty — it never falls back to a `/tmp` guess (that `/tmp` fallback belongs only to `home_dir`, which backs the explicit CLI path). `orchestrator setup` (in `src/main.rs` / `orchestrator_ext.rs`) remains the **optional explicit path** — it wires `pi`-on-PATH detection + the default dir to `materialize` — but is no longer required for normal use.

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
- Real-agent e2e (`tests/e2e_pi_orchestrator.rs`, `#[cfg(feature = "e2e")]`): a real Pi orchestrator delegates to a real worker and receives `work-done`. Pi authenticates to **OpenRouter** (`--provider openrouter --model <gpt-5.x>`) via `OPENROUTER_API_KEY`, sourced through `vals`/`.env.vals.yaml` (`ref+gcpsecrets://vfarcic/open-router-key`). The TUI harness `env_clear`s the child, so the test explicitly propagates `OPENROUTER_API_KEY` + `HOME` to the Pi child.
