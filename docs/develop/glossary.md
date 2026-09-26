# Glossary: one word per concept

dot-agent-deck is one product with two clients — the terminal TUI and the desktop app — talking to one daemon over one protocol (CLAUDE.md rule 18). This page fixes **one word for each concept** so the two clients and the protocol say the same thing. It was settled under issue #1045, whose rule from the maintainer was: **use the TUI's words.** Where the TUI itself used more than one word, the maintainer picked one; where the TUI had no word at all, the orchestrator of that issue picked one and the PR is where to push back. The table is the target both clients and the protocol are held to: a surface that still shows a word from the **Replaces** column is a bug against this page.

## How to use this

- **A new surface uses the glossary word.** That applies to rendered text in either client, to CLI help, to user-facing docs, and to new protocol names (ops, capability strings, response fields). If the concept you are naming is not here, either it is new — add a row in the same PR — or it is one of the rows below under a word you did not recognise.
- **Code identifiers follow the same word where the code is new.** Existing identifiers are renamed only when a change is already touching them or when the old word is itself misleading; a rename-for-its-own-sake of a stable identifier is churn. Wire names are the exception that needs care: renaming one is a protocol change and goes through CLAUDE.md rule 18's graded path, as `prepare-orchestration` did (below).
- **PRD #741 M15's naming rule is superseded.** M15 decided that the desktop's rendered text says "Deck" for the daemon while the code keeps `daemon`. #1045 replaced that: rendered text says **daemon** too, and "deck" is kept for the product (see [Homonyms](#homonyms)). `prds/741-desktop-connect-any-daemon.md` carries a note at the rule pointing here.
- **A homonym is not a synonym.** Several words below legitimately mean two things (see [Homonyms](#homonyms)). Do not "fix" one meaning by renaming the other.

## Canonical words

| Concept | Canonical word | Replaces | TUI | Desktop | Protocol / CLI |
| --- | --- | --- | --- | --- | --- |
| A named set of roles from `[[orchestrations]]` in `.dot-agent-deck.toml`, spun up together | **orchestration** | desktop "workflow" and "live loop"; the protocol's `prepare-workflow` / `workflow_prepared` / `PreparedWorkflow` | "Activated orchestration", the `Orch:` chip and "orch tab" (abbreviations of the same word) | the Orchestrations panel and launch flow | `PrepareOrchestration` (op `prepare-orchestration`), `PreparedOrchestration`, `orchestration_prepared`, `OrchestrationSurface`, `TabMembership::Orchestration` |
| The background process that owns agents, PTYs, session state and hooks | **daemon** | desktop "Deck"/"deck" (PRD #741 M15), "control service"; daemon prose that called itself "the deck" | "shut down agents and daemon", "Stop failed … restart the daemon" | "Start daemon", "Stop daemon", "Replace daemon", "Daemon disconnected" | `dot-agent-deck daemon …`, `stop-daemon`, `DaemonClient`, `daemon_version` |
| A configured endpoint — local or remote — that a client can reach | **daemon** | desktop "Deck" in the selector and settings, "Decks", "All Decks", "Local deck"/"Remote deck" | the TUI attaches to one daemon at a time and only has a word for the remote case, "remote" | "Daemon" selector, "Daemons" settings section, "All daemons", "Local daemon"/"Remote daemon", the Daemons screen | `Endpoint`, `EndpointIdentity`, `dot-agent-deck daemon endpoint`, `dot-agent-deck remote …` |
| One running agent | **agent** | TUI "session", and "pane"/"card" where they name the agent itself | "No active agents", "{n} agent(s)", "Filter agents", "Rename agent", "New Agent", "Close agent" | "Create agent", "Close agent" | `AgentRecord`, `start-agent`, `stop-agent` |
| The terminal view of an agent | **pane** | — | "Focus selected pane", an embedded pane disconnecting | the agent's terminal pane | `pane_id`, `dot-agent-deck pane …` |
| An agent's tile on the dashboard | **card** | — | "Select next card", "Jump to card N" | the agent tile | — |
| The screen listing every agent | **Dashboard** | desktop "Overview", "Agent overview" | the dashboard, and the **Command Mode** state that shows it | the Dashboard rail entry and heading | `TabMembership` `None` ("the dashboard pane") |
| The TUI input state in which keys navigate the deck | **command mode** | TUI "Normal mode" | "COMMAND MODE" banner, "Command Mode" button, "Return to command mode" | — (TUI only) | `UiMode` (Rust only) |
| The TUI input state in which keys go to the focused pane | **PaneInput mode** | — | "PaneInput mode" | — (TUI only) | `UiMode` (Rust only) |
| The role marked `start = true` | **orchestrator** | desktop COORDINATOR badge and "coordinator"; daemon prose "coordinator context" | "Orchestrator prompt not delivered …", `(orchestrator)` in `daemon status` | the orchestrator badge, "Message orchestrator…" | `is_start_role`, `orchestrator_pane_ids`, the orchestrator context file |
| A member of an orchestration | **role** (a non-start role is a **worker**) | — (already aligned) | role | role | `role_name`, `ProjectRole` |
| Agent status `WaitingForInput` | **Needs Input** | the TUI stats bar's "waiting" | the card status and the stats bar ("2 needs input") | pending #1043 (see [Exceptions](#known-exceptions)) | `SessionStatus::WaitingForInput` |
| The orchestrator handing work to a role | **delegate** / **delegation** | desktop "HANDOFFS", "handoff", and the edge status "dispatched" | `dot-agent-deck delegate` | "DELEGATIONS", "Live delegations", "delegated" | the `delegate` message and CLI |
| What agents report while they work | **event** | desktop "Evidence", "Transition evidence", "Run evidence", "EVENT LEDGER" | hook events | "Events", the events drawer | `AgentEvent`, `EventType`, `subscribe-events` |
| Ending one agent | **Close** | desktop "Stop agent" | "Close agent", "Close selected agent?" | "Close agent" | `stop-agent` (the verb on the wire is unchanged) |
| Ending the daemon | **Stop** | — | the quit dialog's Stop, "shut down agents and daemon" | "Stop daemon" | `stop-daemon`, `dot-agent-deck daemon stop` |
| Bringing up one agent | **New Agent** / **create** | desktop "Start agent" | "New Agent", "Created agent {id}" | "Create agent" | `start-agent` |
| Bringing up an orchestration | **activate** | desktop "Start orchestration", "Launch live loop", "Launch {name}?", "launch a workflow" | "Activated orchestration: {name}" | "Activate orchestration", "Activate {name}?" | `prepare-orchestration` then `start-prepared-agent` per role |
| Where an agent runs / a configured project | **directory** / **project** | desktop "repositories" | `Dir:`, project | "Choose projects & orchestrations" | `cwd`, `path`, `ListProjects`, `ListDirectories` |
| Cron-scheduled prompts | **schedule** | TUI "Scheduled Tasks" | "Schedules", the Schedules manager | the schedule authoring option | `dot-agent-deck schedule` |

### Checked and already aligned

These were compared across both clients and the protocol and needed no change; they are recorded so nobody re-opens them without a reason.

- **Product name** — `dot-agent-deck` in the TUI and CLI, "Agent Deck" in the desktop. Branding, not a terminology divergence.
- **prompt** vs **task** — two concepts, not two words for one: a prompt is text sent to an agent, a task is a unit of work handed to an orchestration.
- **connect** / **detach** — the same in both clients.
- **rename** — the verb matches; its object is **agent** (above).
- **tab** (TUI) vs **group** (desktop) — different presentations of an orchestration's or mode's agents, not two words for one thing. The wire name `TabMembership` stays.
- **workspace mode** (`[[modes]]`) — "mode" in both clients.
- **Agent Profiles** and **fleet** — desktop-only features with no TUI counterpart. The multi-daemon view is "All daemons".

## Known exceptions

Places that deliberately keep an old word, and why.

- **`DOT_AGENT_DECK_COORDINATION_RETENTION_DAYS`.** An environment variable a user may already have set; renaming it would silently stop honouring their value. Kept as spelled. The concept it tunes is the orchestrator's coordination files.
- **The legacy wire name `prepare-workflow`.** Issue #1045 renamed the prepare verb on the wire to `prepare-orchestration` without a `PROTOCOL_VERSION` bump, by the capability-gated rung of rule 18. The daemon still answers the old op and still advertises the old capability, so a desktop built before the rename keeps working against a newer daemon; the client library sends the new op when the daemon advertises `prepare-orchestration` and falls back to `prepare-workflow` when it advertises only that, so a newer desktop keeps working against an older daemon. Each spelling is answered on its own response field — `orchestration_prepared` for the new op, `workflow_prepared` for the old — and a reader accepts either. The legacy op, capability and field are retired together in the change that next bumps `PROTOCOL_VERSION`; the header of `src/daemon_protocol.rs` and the doc on `CAP_PREPARE_WORKFLOW` say why that is the point they stop being reachable.
- **The desktop's status vocabulary.** The desktop collapses agent status into its own set (running, waiting, failed, …) and that derivation belongs to issue #1043, not to this glossary. Until #1043 lands, "Needs Input" is the canonical word for `WaitingForInput` in the TUI and in anything new, and the desktop's existing status words are not renamed piecemeal.
- **`TabMembership`.** A wire name, and "tab" is the TUI's presentation of it; see *tab vs group* above.

## Homonyms

One spelling, more than one concept. Each meaning is legitimate; read the word from its context rather than renaming one meaning to rescue the other.

- **session** — (a) the saved layout the TUI restores ("Session — Panes auto-saved continuously", "failed to save session", `snapshot`); (b) the agent CLI's own session (`SessionStart` / `SessionEnd` hook events, "Session started"). The word for a running agent is **agent**, not session.
- **mode** — workspace mode (`[[modes]]`), input mode (command mode / PaneInput mode), and layout mode ("Layout: {mode}").
- **workflow** — in the desktop, the pipeline of stages and nodes that issue #1043 owns (`workflow-node`, "No workflow nodes reported", `WorkflowStage`). The word for an orchestration is **orchestration**, not workflow.
- **dispatch** — the TUI's `dot-agent-deck dispatch` verb, which creates a worktree and an isolated line of work. It is not the word for handing work to a role; that is **delegate**, which is why the desktop's "dispatched" edge status became "delegated".
- **run** — the desktop's Runs screen and its run vocabulary belong to #1043.
- **deck** — the product (`dot-agent-deck`, "Agent Deck") and, in TUI and CLI prose, the installed binary ("checking the remote deck", "as a deck environment"). The word for the process and for a configured endpoint is **daemon**.
