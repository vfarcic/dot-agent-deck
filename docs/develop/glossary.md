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
| A named set of roles from `[[orchestrations]]` in `.dot-agent-deck.toml`, spun up together | **orchestration** | desktop "workflow" and "live loop"; the protocol's `prepare-workflow` / `workflow_prepared` / `PreparedWorkflow` | "Activated orchestration", the `Orch:` chip and "orch tab" (abbreviations of the same word) | the Orchestrations rail entry and panel, "Activate orchestration" | `PrepareOrchestration` (op `prepare-orchestration`), `PreparedOrchestration`, `orchestration_prepared`, `OrchestrationSurface`, `TabMembership::Orchestration` |
| The background process that owns agents, PTYs, session state and hooks | **daemon** | desktop "Deck"/"deck" (PRD #741 M15), "control service"; daemon prose that called itself "the deck" | "shut down agents and daemon", "Stop failed … restart the daemon" | "Start daemon", "Stop daemon", "Replace daemon", "Daemon disconnected" | `dot-agent-deck daemon …`, `stop-daemon`, `DaemonClient`, `daemon_version` |
| A configured endpoint — local or remote — that a client can reach | **daemon** | desktop "Deck" in the selector and settings, "Decks", "All Decks", "Local deck"/"Remote deck" | the TUI attaches to one daemon at a time and only has a word for the remote case, "remote" | "Daemon" selector, "Daemons" settings section, "All daemons", "Local daemon"/"Remote daemon", the Daemons screen | `Endpoint`, `EndpointIdentity`, `dot-agent-deck daemon endpoint`, `dot-agent-deck remote …` |
| One running agent | **agent** | TUI "session", and "pane"/"card" where they name the agent itself | "No active agents", "{n} agent(s)", "Filter agents", "Rename agent", "New Agent", "Close agent" | the New agent button and dialog, "Create agent", "Close agent" | `AgentRecord`, `start-agent`, `stop-agent` |
| The terminal view of an agent | **pane** | — | "Focus selected pane", an embedded pane disconnecting | the agent's terminal pane | `pane_id`, `dot-agent-deck pane …` |
| An agent's tile on the dashboard | **card** | — | "Select next card", "Jump to card N" | the agent tile | — |
| The screen listing every agent | **Dashboard** | desktop "Overview", "Agent overview" | the dashboard, and the **Command Mode** state that shows it | the Dashboard rail entry, and "Back to dashboard" on an agent's full-window view | `TabMembership` `None` ("the dashboard pane") |
| The TUI input state in which keys navigate the deck | **command mode** | TUI "Normal mode" | "COMMAND MODE" banner, "Command Mode" button, "Return to command mode" | — (TUI only) | `UiMode` (Rust only) |
| The TUI input state in which keys go to the focused pane | **PaneInput mode** | — | "PaneInput mode" | — (TUI only) | `UiMode` (Rust only) |
| The role marked `start = true` | **orchestrator** | desktop COORDINATOR badge and "coordinator"; daemon prose "coordinator context" | "Orchestrator prompt not delivered …", `(orchestrator)` in `daemon status` | the ORCHESTRATOR badge, "Message orchestrator…" | `is_start_role`, `orchestrator_pane_ids`, the orchestrator context file |
| A member of an orchestration | **role** (a non-start role is a **worker**) | — (already aligned) | role | role | `role_name`, `ProjectRole` |
| Agent status `WaitingForInput` | **Needs Input** | the TUI stats bar's "waiting" | the card status and the stats bar ("2 needs input") | pending #1043 (see [Exceptions](#known-exceptions)) | `SessionStatus::WaitingForInput` |
| The orchestrator handing work to a role | **delegate** / **delegation** | desktop "HANDOFFS", "handoff", and the edge status "dispatched" | `dot-agent-deck delegate` | "DELEGATIONS", "Live delegations", "delegated" | the `delegate` message and CLI |
| What agents report while they work | **event** | desktop "Evidence", "Transition evidence", "Run evidence", "EVENT LEDGER" | hook events | "Events", the events drawer | `AgentEvent`, `EventType`, `subscribe-events` |
| Ending one agent | **Close** | desktop "Stop agent" | "Close agent", "Close selected agent?" | "Close {name} agent" on a dashboard row, its confirmation "Close {name}?" / "Close agent"; "Close {name} orchestration" and "Close all N roles" for an orchestration | `stop-agent` (the verb on the wire is unchanged) |
| Ending the daemon | **Stop** | — | the quit dialog's Stop, "shut down agents and daemon" | "Stop daemon" | `stop-daemon`, `dot-agent-deck daemon stop` |
| Bringing up one agent | **New Agent** / **create** | desktop "Start agent" | "New Agent", "Created agent {id}" | "New agent", "Create agent" | `start-agent` |
| Bringing up an orchestration | **activate** | desktop "Start orchestration", "Launch live loop", "Launch {name}?", "launch a workflow" | "Activated orchestration: {name}" | "Activate orchestration", "Activate {name}?", "activate again" | `prepare-orchestration` then `start-prepared-agent` per role |
| Where an agent runs / a configured project | **directory** / **project** | desktop "repositories" | `Dir:`, project | "Choose projects & orchestrations" | `cwd`, `path`, `ListProjects`, `ListDirectories` |
| Cron-scheduled prompts | **schedule** | TUI "Scheduled Tasks" | "Schedules", the Schedules manager | the schedule authoring option | `dot-agent-deck schedule` |

### Ending an agent versus leaving its view

Two desktop controls act on an agent from different places: the dashboard row's control ends the agent, and the full-window agent view's control only dismisses the view. Issue #1045 named them by the glossary's words. The row's control is **"Close {name} agent"**, because **Close** is the word for ending one agent; the full-window view's dismiss is **"Back to dashboard"**, because it ends nothing and the Dashboard is where it returns to. The busy label while a close is in flight stays "Stopping…": it describes what the daemon is doing, not the name of the action.

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
- **The `[scheduled_tasks]` config key.** The table in the global config that holds schedules, and its `[scheduled_tasks.issue_dispatch]` sub-table, keep the old spelling. They are keys in a file a user has already written; renaming them would stop the daemon finding schedules an older build wrote, which is a compatibility change and not a wording one. Everything a user reads — the Schedules manager, `dot-agent-deck schedule` output and help, and config validation messages — says **schedule**.
- **The legacy wire name `prepare-workflow`.** Issue #1045 renamed the prepare verb on the wire to `prepare-orchestration` without a `PROTOCOL_VERSION` bump, by the capability-gated rung of rule 18. The daemon still answers the old op and still advertises the old capability, so a desktop built before the rename keeps working against a newer daemon; the client library sends the new op when the daemon advertises `prepare-orchestration` and falls back to `prepare-workflow` when it advertises only that, so a newer desktop keeps working against an older daemon. Each spelling is answered on its own response field — `orchestration_prepared` for the new op, `workflow_prepared` for the old — and a reader accepts either. The legacy op, capability and field are retired together in the change that next bumps `PROTOCOL_VERSION`; the header of `src/daemon_protocol.rs` and the doc on `CAP_PREPARE_WORKFLOW` say why that is the point they stop being reachable.
- **The desktop's status vocabulary.** The desktop collapses agent status into its own set (running, waiting, failed, …) and that derivation belongs to issue #1043, not to this glossary. Until #1043 lands, "Needs Input" is the canonical word for `WaitingForInput` in the TUI and in anything new, and the desktop's existing status words are not renamed piecemeal.
- **`TabMembership`.** A wire name, and "tab" is the TUI's presentation of it; see *tab vs group* above.
- **Desktop identifiers that keep an old word.** The rendered text follows the table; these names in code, CSS, test selectors and JSON were kept by decision, because renaming them is churn that the e2e selectors and the voice layer would have to follow, and the words a user reads next to them are the glossary's. Rename one when a change is already touching it (see [How to use this](#how-to-use-this)).
  - **delegation:** the `HandoffRail` component, the `handoff-*` classes and test ids (`handoff-rail`, `handoff-edge`, `handoff-status`, …), and `HANDOFF_EVENTS` / `MAX_LIVE_HANDOFFS`. The rail renders "DELEGATIONS" and "Live delegations".
  - **event:** the `evidence-*` classes and test ids (`evidence-drawer`, `evidence-list`, `evidence-detail`, …), `toggleEvidenceDrawer` and `evidenceOpen`. The drawer renders "Events".
  - **orchestrator:** the `messageCoordinator` voice action and the `.coordinator-badge` class. They render "Message orchestrator…" and "ORCHESTRATOR".
  - **daemon:** the `deck-*` test ids (`deck-selector-*`, `deck-result-*`, `open-deck`, …); `deckId`, `deckKind`, `deckName`, `DeckSelector`, `DeckFleet`, `DeckShell`, `ALL_DECKS_SELECTION` and the settings section id `decks`; and the `deck-` prefix of the token `EndpointIdentity::wire_id()` mints (`deck-<16 hex digits>`, in `src/daemon_client.rs`).
  - **dashboard:** the `AgentOverview` component, the `overview-*` and `open-overview` test ids, and the voice ids `open_overview` / `openOverview`.
  - **voice JSON and ids:** the prompt key `decks` (the list of daemons the model may name), the parameter kind `deck_ref` and the parameter `deck`, the voice rows `open_deck` and `choose_deck`, and the screen ids `deck` and `overview` (`Screen::Deck`). These are part of the schema the voice model is prompted with, and the instructions it is given say that a user may still call a daemon a deck.
- **Old labels accepted as spoken input.** The voice layer's command table (`desktop/src-tauri/src/voice/commands.toml`) still lists pre-#1045 words — "open deck", "go back to the deck", "deck build box" (the New agent dialog's Daemon field, `choose_deck`), "start orchestration" — among the phrases for the renamed controls, so a user who learned the old words is not refused. They are phrases the voice layer matches, not labels; the controls themselves render the glossary words.

## Homonyms

One spelling, more than one concept. Each meaning is legitimate; read the word from its context rather than renaming one meaning to rescue the other.

- **session** — (a) the saved layout the TUI restores ("Session — Panes auto-saved continuously", "failed to save session", `snapshot`); (b) the agent CLI's own session (`SessionStart` / `SessionEnd` hook events, "Session started"); (c) the identity of the output stream a pane is attached to, which the TUI's input-delivery messages check before they send keystrokes ("History-only session cannot accept live input", "Input not delivered: session is history-only", "Input not delivered: the pane's session changed", and the "history-only session" / "wrong session" reasons in `src/ui.rs`). Sense (c) is the target the daemon writes input into (`Writable` and `SendResult` in `src/event.rs`): a history-only session — a wrapped Codex agent, for one, whose keystrokes reach it through the terminal it inherited rather than a handle the daemon holds — cannot take input from the dashboard, and a send whose handle no longer maps to the session it was meant for is reported as the wrong session. The strings keep the word because they describe that stream rather than the agent. The word for a running agent is **agent**, not session.
- **mode** — workspace mode (`[[modes]]`), input mode (command mode / PaneInput mode), and layout mode ("Layout: {mode}").
- **workflow** — in the desktop, the pipeline of stages and nodes that issue #1043 owns (`workflow-node`, "No workflow nodes reported", `WorkflowStage`). The word for an orchestration is **orchestration**, not workflow.
- **dispatch** — the TUI's `dot-agent-deck dispatch` verb, which creates a worktree and an isolated line of work. It is not the word for handing work to a role; that is **delegate**, which is why the desktop's "dispatched" edge status became "delegated".
- **run** — the desktop's Runs screen and its run vocabulary belong to #1043.
- **deck** — the product (`dot-agent-deck`, "Agent Deck") and, in TUI and CLI prose, the installed binary ("checking the remote deck", "as a deck environment"). The word for the process and for a configured endpoint is **daemon**.
