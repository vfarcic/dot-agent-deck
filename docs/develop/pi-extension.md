# Pi Orchestrator Extension

> **Developer / maintainer reference.** This page documents the internal contract of the bundled Pi extension and is intentionally excluded from the published documentation site.

[Pi](https://github.com/earendil-works/pi) is integrated as a first-class agent (PRD #201). The user-facing setup lives in the published [Pi Agent](../pi-agent.md) page; this page is the contract for maintainers.

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

## Materialization (`src/orchestrator_ext.rs`)

`index.ts` and `orchestrator.ts` are embedded with `include_str!` pointed at the **real** `pi-extension/src/` files (no fork — editing the extension flows into the binary on rebuild). `materialize(target_dir)` writes them into Pi's subdir discovery layout `<dir>/index.ts` + `<dir>/orchestrator.ts`; the real target is `~/.pi/agent/extensions/dot-agent-deck/`. `package.json` is intentionally not embedded — Pi's subdir `index.ts` discovery needs none, and TypeBox resolves from Pi's runtime. `orchestrator setup` (also in `src/main.rs` / `orchestrator_ext.rs`) wires `pi`-on-PATH detection + the default dir to `materialize`.

## No hooks for a Pi pane

A Pi pane installs no Claude Code hook and mutates no `~/.claude/settings.json` — **by construction, with zero gating code.** `hooks_manage::auto_install()` runs only at TUI/dashboard startup, is machine-global, and takes no `AgentType`; the daemon-serve / spawn / scheduler / `agent-event` paths never call it. Design Decision #4 is satisfied without any `AgentType::Pi` branch in `src/hooks_manage.rs`.

## Cross-version contract (rule 12)

The `agent-event` addition is an **additive CLI subcommand over the existing `AgentEvent` wire** — no wire shape or field-meaning change. Classification: **no `PROTOCOL_VERSION` bump, no `.breaking.md`.** See [versioning](versioning.md).

## Experimental gating

The Pi surface is gated behind `experimental` via the single wrapper `features::show_pi_agent()`, applied only at the render seam in `src/ui.rs` `render_session_card` (an off-flag Pi card falls back to the pre-feature `AgentType::None` placeholder without hiding a running pane). Business logic, the daemon protocol, hooks, the extension, and `agent-event` routing are **not** gated. See [experimental-flag](experimental-flag.md). Graduation issue: `graduate-pi-agent` (`grep show_pi_agent` finds every call site).

## Testing

- Fast tier: the agent-agnostic synthetic harness (`tests/common/synthetic_agent.rs`) exercises delegate/work-done/`agent-event` routing parameterized by agent identity (Pi row instantiated here; the companion cross-agent PRD adds `claude`/`opencode`).
- TS unit tests: `cd pi-extension && npm test`.
- Real-agent e2e (`tests/e2e_pi_orchestrator.rs`, `#[cfg(feature = "e2e")]`): a real Pi orchestrator delegates to a real worker and receives `work-done`. Pi authenticates to **OpenRouter** (`--provider openrouter --model <gpt-5.x>`) via `OPENROUTER_API_KEY`, sourced through `vals`/`.env.vals.yaml` (`ref+gcpsecrets://vfarcic/open-router-key`). The TUI harness `env_clear`s the child, so the test explicitly propagates `OPENROUTER_API_KEY` + `HOME` to the Pi child.
