# dot-agent-deck Pi extension

The bundled Pi orchestrator extension for dot-agent-deck (PRD #201). It makes a **Pi** agent a first-class, deterministically-orchestrated node in a dot-agent-deck orchestration by:

1. **Native tools** — registering `delegate` and `work_done` as schema-validated Pi tools whose bodies shell the existing `dot-agent-deck` CLI (instead of prompting the model to type the CLI string itself), and
2. **Event-driven status** — subscribing to Pi's lifecycle event bus and reporting the pane's status via `dot-agent-deck agent-event --type <running|waiting|finished>`, so a Pi pane is status-tracked with **no Claude-Code hook installed** and **no `~/.claude/settings.json` mutation**.

Both paths route over the daemon socket using the pane env vars the daemon already injects (`DOT_AGENT_DECK_PANE_ID` / `DOT_AGENT_DECK_AGENT_ID` / `DOT_AGENT_DECK_VIA_DAEMON`). The extension reads them via the CLI; it does not set them.

This directory contains the **entire** JS/TS toolchain for the extension and is kept off the Rust critical path (`cargo`/`nextest` never touch it). Bundling into the binary (`include_str!`) and materialization into Pi's extension dir are later milestones (M3.1/M3.2).

## Layout

```
pi-extension/
├── src/
│   ├── orchestrator.ts   # PURE logic (zero imports): argv builders, Pi-event→state
│   │                     #   mapping, exec-error classification — the unit-test target
│   └── index.ts          # Pi-API glue: default factory wiring the pure logic to
│                         #   pi.registerTool() / pi.on() and pi.exec()
├── test/
│   └── orchestrator.test.ts  # node:test unit tests (PRD #201 test-plan rows 8 & 9)
├── package.json          # test toolchain (tsx) + the `pi.extensions` entry point
├── tsconfig.json
└── README.md
```

The pure/glue split mirrors the Rust side's pure `agent_event_type_from_state` seam: the canonical status vocabulary (`running` / `waiting` / `finished`) lives in one place and everything maps into it.

## Running the tests

```bash
cd pi-extension
npm install     # installs tsx (the only dependency)
npm test        # node --import tsx --test test/*.test.ts
```

The tests import only the import-free `src/orchestrator.ts`, so they need **no running `pi`** and none of the Pi packages installed.

## Dependencies

- **`tsx`** (dev) — the only installed dependency; lets Node's built-in test runner execute the TypeScript tests.
- **`@earendil-works/pi-coding-agent`** and **`typebox`** — imported by `src/index.ts` for types and tool schemas. These are **not** listed as dependencies here because Pi resolves them from its own installation when it loads the extension (via jiti). Optional local type-checking (`npm run typecheck`) therefore requires Pi installed alongside.

## Pi extension API used (Pi 0.80.6)

- **Entry point:** default-export factory `(pi: ExtensionAPI) => void`.
- **Tools:** `pi.registerTool({ name, label, description, promptSnippet, promptGuidelines, parameters: Type.Object({...}), execute })`. A tool signals failure by **throwing** from `execute`; the return value never sets the error flag.
- **Shelling the CLI:** `pi.exec(command, args, { signal })` → `{ stdout, stderr, code, killed }` (resolves with a non-zero `code` on command failure; may reject on spawn failure such as ENOENT).
- **Status events:** `pi.on("session_start" | "agent_start" | "agent_settled" | "session_shutdown", handler)`.

### Pi lifecycle event → status mapping

| Pi event | `agent-event --type` | Meaning |
|---|---|---|
| `session_start` | `waiting` | Agent is up, awaiting the first prompt |
| `agent_start` | `running` | An agent run has begun |
| `agent_settled` | `waiting` | Pi will not continue automatically; awaiting input |
| `session_shutdown` | `finished` | The Pi session is exiting |

`agent_end` is deliberately **not** mapped — after it, Pi may still auto-retry, auto-compact, or drain queued follow-up messages, so it is not a reliable idle signal; `agent_settled` is. Every other event is ignored (no `agent-event` emitted), so a bogus `--type` can never reach the CLI.
