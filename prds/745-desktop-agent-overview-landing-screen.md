# PRD #745: An agent overview as the desktop app's landing screen

**Status**: Not started
**Priority**: Medium
**Created**: 2026-08-30
**Issue**: [#745](https://github.com/vfarcic/dot-agent-deck/issues/745)

## Problem Statement

The desktop app opens straight into the control deck. `App` is two lines — `return <ControlDeck runtime={useDeckRuntime()} />` (`desktop/src/App.tsx:63-65`) — and `ControlDeck` renders one flat `.agent-grid` of every agent the daemon owns (`desktop/src/App.tsx:383-404`), each tile defaulting to a mounted terminal (`tabs[agent.id] ?? "terminal"`, `desktop/src/App.tsx:390`). There is no index, no "what is running right now" summary, and nothing that degrades gracefully once there are more agents than fit on screen. A daemon commonly runs nine or more.

Three things make that worse than a layout complaint.

**The deck renders everything, always.** Nothing virtualises or paginates. Each visible tile allocates an xterm with 8000 lines of scrollback and attempts a WebGL context (`desktop/src/components/TerminalViewport.tsx:61,93-104`); browsers cap concurrent WebGL contexts, and past the cap the addon's `catch` silently degrades that pane to the DOM renderer (`:101-104`). PRD #176's M1.3 throughput qualification — the milestone that was supposed to measure exactly this — is still outstanding (`prds/176-desktop-gui.md:123`), and no benchmark exists anywhere in the repo.

**Attaching a PTY is not tied to rendering one.** This is the finding that most shapes this PRD. `TerminalViewport` never attaches anything; the attach trigger is `TauriDeckBridge.attachAgents`, which attaches every agent in the snapshot that is not already attached (`desktop/src/lib/bridge.ts:367-373`), fired from `connect()` (`:488`) and again on **every** `desktop://snapshot` event (`:506`). Each attach opens a real daemon `AttachStream` socket (`desktop/src-tauri/src/terminal.rs:119-137`), and an `AttachStream` sends a full scrollback snapshot before it starts streaming (`src/daemon_protocol.rs:60-65`). So a list-only overview that mounts no terminal at all would still open nine sockets and replay nine scrollbacks today, for content nobody is reading.

**The grouping the screen wants is already delivered and thrown away.** The daemon sends each agent's tab membership — dashboard, mode name, or orchestration with role name, role index, start-role flag, orchestration id and display title — and it survives all the way into the webview (`desktop/src/lib/bridge.ts:45-48`). `agentFromDto` then reduces it to a role string and two booleans and drops the rest (`:143-152,179-180`); `AgentSession` has no `tab` field at all (`desktop/src/types.ts:58-88`). Nine agents belonging to two orchestrations currently read as nine unrelated tiles.

## Solution Overview

The app lands on a fleet overview: every agent the daemon runs, grouped the way the daemon already groups them, described only by things that are actually true, with no terminal attached. You drill into an agent or an orchestration from there, and only then does a PTY get attached.

Three commitments define the screen.

**Honesty over completeness.** The overview shows what the daemon genuinely knows and nothing else. It is explicitly better to ship a narrow screen than a wide one padded with `Unavailable` — see [Columns: the central decision](#columns-the-central-decision), which settles this field by field. This extends to values the app currently *fabricates*: live mode renders `ATT 01` on every tile from a hardcoded `attempt: 1` (`desktop/src/lib/bridge.ts:163`, `desktop/src/components/AgentTile.tsx:135-138`), which is worse than showing nothing because it looks like data.

**No terminal on this screen, ever.** Drill-in is what attaches. That requires making attach demand-driven first, which is the prerequisite milestone rather than an optimisation.

**Single-daemon today, daemon-shaped from day one.** The screen is written against one daemon, but the model carries a daemon identity and the layout's top-level unit is a daemon group — see [Preserving the multi-daemon extension](#preserving-the-multi-daemon-extension).

## Scope

### In Scope

- **Demand-driven terminal attach.** Attach on drill-in rather than on every snapshot. This is a behaviour change to the existing deck as well, and is the prerequisite for everything else here.
- **The overview screen itself**: grouped agent cards, honest columns, live status.
- **Grouping by the daemon's own tab buckets**: an orchestration reads as one unit with its roles in role order and its coordinator marked; mode tabs group by mode name; dashboard agents form a standalone bucket.
- **Navigation**: land on the overview, drill into an agent or an orchestration, return to the overview.
- **View state above `ControlDeck`**, so the overview renders *instead of* the deck rather than inside it.
- **A daemon identity on the agent model** and composite `(daemonId, agentId)` keying, while there is still exactly one daemon and it costs nothing.
- **Empty, unreachable and incompatible states**, including a genuine "connected, zero agents" first-run state — and the fixture for it, which does not currently exist.
- **Surfacing three fields already on the wire but dropped by the desktop's own Rust DTO**: write lease, last user prompt, orchestration cwd. No protocol change; see [Cross-version safety](#cross-version-safety).
- **Removing the two fabricated live-mode values** (`attempt`, `branch`) from what the app presents as fact.
- **`last_activity` as an additive optional field on `SessionSnapshot`** — the one daemon-side change in this PRD, sequenced last and severable. See M7 and Open Question 4.

### Out of Scope

- **Model, token, cost and context-window columns.** They do not exist in daemon state at all — not on the wire, not in `RunningAgent` (`src/agent_pty.rs:1688-1846`), not in `SessionState` (`src/state.rs:670-707`). Creating them is [PRD #633](https://github.com/vfarcic/dot-agent-deck/issues/633)'s discovery work, not this screen's.
- **Git branch per agent.** Nothing tracks it daemon-side; the only `git branch` calls in `src/` are deletions in the dispatch flows. Reconstructing it means either a subprocess per agent cwd on the daemon, or a desktop-side git call that breaks the "local daemons only" boundary (`docs/develop/desktop-gui.md:97`).
- **Attempt/retry count.** No such counter exists anywhere. Removing the fabricated one is in scope; inventing a real one is not.
- **Session duration.** `SessionState.started_at` exists but is invented as `now` on hydration (`src/state.rs:5684-5694`), so any duration resets when the daemon restarts under a live agent. A duration that silently lies about long-running work is worse than no duration.
- **Terminal thumbnails, previews or output snippets on the overview.** Any of them re-creates the attach cost this PRD exists to remove.
- **Connecting to more than one daemon.** That is [#742](https://github.com/vfarcic/dot-agent-deck/issues/742). This PRD only owes it a layout and a key that do not have to be rewritten.
- **Virtualisation or pagination of the overview.** Cards without terminals are cheap; revisit if a real deployment exceeds what one screen holds.
- **The `experimental` feature flag.** Deliberately not applied — see [Feature flag](#feature-flag).
- **Any end-to-end harness for the real Tauri window.** None exists (see [Testing](#testing-what-rule-4-means-here)); building one is its own PRD.

## Technical Approach

### Columns: the central decision

The issue is emphatic that this be decided deliberately, so it is decided here rather than deferred. **The overview shows only what the daemon genuinely exposes. No column is added to this PRD that would require inventing a new source of truth.**

Available today with **no daemon change and no protocol change** — all of it already reaches the webview:

| Column | Source | Note |
|---|---|---|
| Status | `SessionSnapshot.status` (`src/state.rs:642`) → `DAEMON_STATUS` (`desktop/src/lib/bridge.ts:128-140`) | The primary signal. Already exhaustively mapped, with an unknown status falling through to a neutral state rather than a terminal one. |
| Display name | `AgentRecord.display_name` (`src/agent_pty.rs:1968`) | Falls back to role. |
| Agent type / CLI | `AgentRecord.agent_type` (`src/agent_pty.rs:1992`), live value preferred (`desktop/src-tauri/src/dto.rs:270-275`) | |
| Working directory | `AgentRecord.cwd` (`src/agent_pty.rs:1973`) | Labelled as the working directory, **not** as "worktree" — the daemon has no per-agent worktree field. |
| Active tool | `SessionSnapshot.active_tool` (`src/state.rs:650`) | Name plus detail. The live answer to "what is it doing". |
| Tool count | `SessionSnapshot.tool_count` (`src/state.rs:652`) | |
| Orchestration membership | `AgentRecord.tab_membership` (`src/agent_pty.rs:1982`) → `DesktopTab` (`desktop/src-tauri/src/dto.rs:86-107`) | Orchestration name, role name, role index, start-role flag, orchestration id. Present in the webview today and discarded at `desktop/src/lib/bridge.ts:143-152`. Drives grouping. |

Available **on the wire but dropped by the desktop's own Rust DTO** — recovered by editing `map_agent` (`desktop/src-tauri/src/dto.rs:268-302`), with no daemon change and no protocol impact:

| Column | Source | Note |
|---|---|---|
| Last user prompt | `SessionSnapshot.last_user_prompt` (`src/state.rs:654-658`) | The honest replacement for the current hardcoded `"Task metadata unavailable from daemon"` (`desktop/src/lib/bridge.ts:161`). The TUI already renders it as the card's `Prmt:` line (`src/ui.rs:19005-19017`). |
| Write lease | `SessionSnapshot.live_target` (`src/state.rs:666-667`) | Replaces the hardcoded `writeLease: "unknown"` (`desktop/src/lib/bridge.ts:169`). |
| Orchestration cwd | `TabMembership::Orchestration` (`src/agent_pty.rs:332`) | Currently swallowed by the `..` rest-pattern in `map_tab` (`desktop/src-tauri/src/dto.rs:256`). Useful as the orchestration group's header. |

Requiring **daemon work**, and taken only in the last, severable milestone:

| Column | Source | Note |
|---|---|---|
| Last activity | `SessionState.last_activity` (`src/state.rs:678`) | The daemon maintains it and already uses it as the `ListAgents` join tie-breaker (`src/daemon_protocol.rs:1482-1486`), but it is not on `SessionSnapshot`. Added as an additive optional field — a documented **do-not-bump** case (`src/daemon_protocol.rs:11-14`). The desktop-side alternative, accumulating per-agent timestamps from the `desktop://daemon-event` stream, costs nothing but reads blank for every idle agent until it next does something — which on an overview is precisely the agent you most want a timestamp for. |

Explicitly **rejected** as columns, with the reason recorded so this does not get relitigated: model, tokens, cost, context-window percentage, git branch, attempt count, session duration. The first four do not exist in daemon state in any form; branch would need a subprocess per agent or a boundary violation; attempt has no counter; duration cannot survive a daemon restart honestly. The TUI's own session card shows none of them either (`src/ui.rs:18805-19017`), which is independent confirmation that the daemon does not have them rather than that the desktop forgot to ask.

### Demand-driven attach

`attachAgents` moves from "every agent in every snapshot" to "the agents currently being viewed". Concretely: `connect()` (`desktop/src/lib/bridge.ts:488`) and the snapshot listener (`:506`) stop attaching en masse, and attach becomes an explicit call the deck makes for the agent(s) it is showing. Detach on leaving a drill-in is a judgement call — re-attaching replays scrollback from the daemon, so nothing is lost, but a user bouncing between two agents pays that replay each time. Keeping a small most-recently-used set attached is the likely answer; measuring beats guessing here.

All of this is desktop-side. It is also directly testable at the bridge level today: `bridge.test.ts:118-142` already asserts on attach behaviour against a mocked `invoke`, so "the overview attaches nothing" is a real assertion, not an aspiration.

### Grouping

The daemon's `TabMembership` is the grouping key, exactly as the issue suggests:

- **Orchestration** — one card per orchestration, identified by `orchestration_id`, titled by display title or orchestration name, listing its roles in `role_index` order with the start role marked. This is the unit; its members are rows inside it, never peers of it.
- **Mode** — grouped by mode name.
- **Dashboard** — a standalone bucket for agents belonging to neither.

`AgentSession` grows a structured `tab` field so the frontend stops reconstructing membership from a role string. Everything needed is already in `DesktopAgentDto.tab`.

### Screen and view state

There is no router and no view-state concept: every "which surface" decision today is a boolean overlay flag inside `ControlDeck`'s own `useState` block (`desktop/src/App.tsx:69-87`), and the rail's "Runs" button merely closes the panels (`:301`). The overview needs to render *instead of* the deck, so the new state belongs in `App` (`:63-65`), above `ControlDeck` — one discriminated view value (`overview` | drill-in target), not a seventh overlay boolean. No router library is warranted for two views.

### Preserving the multi-daemon extension

Two decisions, both free today, are what stop #742 from being a rewrite.

**The layout's outermost unit is a daemon group, not an agent group.** With one daemon that group renders with minimal chrome — a connection lamp and a socket/identity line, roughly what the rail shows now — and the orchestration/mode groups nest inside it. Adding a second daemon adds a sibling at that level and changes no inner component.

**Agents are keyed by `(daemonId, agentId)` from day one.** Agent ids are per-daemon monotonic integers starting at 1 (`src/agent_pty.rs:3248,4946-4947`), so two daemons both mint `"1"`. Nine separate maps currently key on the bare id, and two of them are sharp: `insert_unique_session` evicts by `agent_id` (`desktop/src-tauri/src/terminal.rs:63`), and `sendTerminalInput` resolves its session by bare `agentId` (`desktop/src/lib/bridge.ts:566`) — which in a multi-daemon world means keystrokes reaching the wrong daemon's agent. This PRD does not fix all nine; it introduces the daemon identity on the model and keys the overview and its drill-in by the composite, so #742 inherits a correct key instead of retrofitting one.

Partial failure — one daemon down while others are healthy — stays out of scope, but nothing here forces the single `ConnectionView` (`desktop/src/types.ts:9-17`) to remain global; it moves inside the daemon group.

### Empty, unreachable and incompatible states

Four states, all of which the daemon already distinguishes and the app already partly renders:

- **Connected, zero agents** — the first-run experience. Today this shows `<EmptyDeck>`'s "No active agent surfaces" with no banner (`desktop/src/App.tsx:405,484-486`). The overview owns this state properly and it must say what to do next. **There is no fixture for it**: `createFixtureSnapshot`'s ternary sends `state=empty` down the *disconnected* branch (`desktop/src/data/fixture.ts:264-268`), so `?fixture=1&state=empty` shows a disconnected banner over an empty deck. Adding a real one is in scope, because otherwise the first-run screen is the one screen with no way to look at it.
- **Disconnected** — banner plus the existing Start daemon / Reconnect actions (`desktop/src/App.tsx:354-360`).
- **Incompatible** — build or protocol mismatch, classified by `classify_handshake` (`desktop/src-tauri/src/daemon_bridge.rs:73-142`). Replace daemon stays gated on a zero-agent daemon (`desktop/src/App.tsx:358`); the overview must not imply a fleet it cannot see.
- **Loading** — the initial seed (`desktop/src/hooks/useDeckRuntime.ts:11-43`).

### Cross-version safety

Per CLAUDE.md rule 12, answered explicitly rather than as a formality.

**M1–M6 do not touch the TUI↔daemon contract at all.** They are frontend changes plus edits to `map_agent`/`map_tab` in `desktop/src-tauri/src/dto.rs`, which is desktop-crate ↔ webview IPC, not the daemon wire. The fields recovered there are already being sent by the daemon and already parsed by the desktop; they are simply not copied into the DTO. No `PROTOCOL_VERSION` bump, no `.breaking.md`, no semantic change to any existing field.

**M7 touches the daemon** by adding `last_activity` to `SessionSnapshot`. `PROTOCOL_VERSION` is currently 8 (`src/daemon_protocol.rs:227`), and the module's own policy names additive optional fields tagged `#[serde(default, skip_serializing_if = "Option::is_none")]` as an explicit **do-not-bump** case (`src/daemon_protocol.rs:11-14`) — the same basis on which `AgentRecord.live` was added (`src/agent_pty.rs:2018-2027`). So M7 needs no bump either, but it does trigger rule 12's **cross-version manual test**: a previous-release daemon with a live agent under it, driven by this branch's TUI, confirming delegation and hooks still work — with `DOT_AGENT_DECK_LOG`, the sockets, `HOME` and `DOT_AGENT_DECK_EXPERIMENTAL` all pinned into the sandbox per rule 12. If M7 is deferred (Open Question 4), the PR's rule 12 answer is an unqualified **no**.

**Bump policy**: patch. Nothing here breaks compatibility in either direction.

### Feature flag

CLAUDE.md rule 9 asks whether a new user-visible surface ships behind `experimental`. The answer here is **no**, and this is not a fresh judgement: PRD #176 decision 6 (`prds/176-desktop-gui.md:101`) already recorded it for this entire binary — *"the `experimental` flag (PRD #139) does not apply. That flag is a presentation switch that gates render/input seams inside the TUI binary… A separate GUI binary has no such seam — the act of building/running it is the opt-in. So maturity is handled by packaging."*

The mechanics confirm it. The desktop crate does link the root library (`desktop/src-tauri/Cargo.toml:15`), so `features::experimental_enabled()` is callable — but nothing calls it, and `run()` never calls `init_and_watch` (`desktop/src-tauri/src/lib.rs:995-1018`), so it would read the OFF default forever. Wiring it up honestly would mean resolving the flag against *some* project directory, and the desktop's only notion of one is `desktop_project_cwd()` — derived from `option_env!("CARGO_MANIFEST_DIR")` with a `current_dir()` fallback (`desktop/src-tauri/src/dto.rs:321-336`), a development convenience rather than a user's project. There is no good answer for a packaged app, which is itself an argument for not gating.

### Testing: what rule 4 means here

Rule 4 mandates coverage for user-visible behaviour and is written in the TUI's vocabulary — L1 (`insta` + `TestBackend`) and L2 (PTY + vt100, `e2e_*.rs`). This feature lands in the Tauri app, so the mapping is stated rather than assumed:

- **The desktop's L1 equivalent exists and is good**: vitest + jsdom + Testing Library, 7 files and 46 tests (`desktop/package.json:14`, `desktop/vite.config.ts:17-21`), rendering the real `ControlDeck` against a hand-built runtime (`desktop/src/App.test.tsx:13-24,43-51`). `TerminalViewport` is already mocked out in that suite (`App.test.tsx:7-9`), so a screen with no terminals is *easier* to cover than the current deck.
- **The Rust DTO half runs in the per-task gate.** `desktop/src-tauri` is a workspace member (`Cargo.toml:2-8`), so its 35 tests run under `cargo test-fast` and are linted by rule 2's clippy command. `agent_mapping_is_frontend_stable` (`desktop/src-tauri/src/dto.rs:533-543`) is the existing pattern for pinning a DTO's exact frontend-facing shape.
- **The desktop's L2 equivalent does not exist.** No Playwright, no WebdriverIO, no `tauri-driver`, nothing under `tests/` that mentions the desktop. Nothing drives the real window, real IPC, real xterm or real WebKitGTK. This PRD does not build that harness, and says so rather than implying rule-4 parity. The compensating control is the manual smoke check in `docs/develop/desktop-gui.md:136`, extended to cover landing on the overview and drilling in against a real daemon.
- **CI**: the `desktop-web` job runs `pnpm test` and `pnpm build` on every non-Renovate PR (`.github/workflows/ci.yml:132-153`), and `pnpm build` is `tsc && vite build` so it is the type gate too. Note `desktop-web` is **not** one of the four required checks — `build`, `build-macos`, `build-windows`, `security` are — so the Rust half of the coverage is the part that actually blocks a merge.

The single most valuable test in this PRD is the bridge-level one asserting that rendering the overview attaches **zero** terminals, because that is the property the whole design rests on and the one that will silently regress.

## Success Criteria

- Opening the app lands on an overview of every agent the daemon runs, not on a deck of terminals.
- Rendering the overview attaches **zero** PTYs, proven by an automated test rather than by inspection. Attaching happens on drill-in and nowhere else.
- Nine agents across two orchestrations read as two units, with roles in role order and each coordinator identifiable — not as nine unrelated tiles.
- Every value on the screen is one the daemon actually reported. Nothing is fabricated, and nothing reads `Unavailable`.
- A daemon that is down, incompatible, or owns zero agents each produce a screen that says what happened and what to do next — and each is reachable in the fixture without a real daemon.
- Drilling into an agent and returning leaves the terminal working, with no lost or duplicated output.
- Adding a second daemon later means adding a sibling group and no inner component changes; nothing in the overview is keyed by a bare agent id.

## Milestones

- [ ] **M1 — Demand-driven attach.** `attachAgents` stops firing for every agent on `connect()` and on every snapshot; attach becomes explicit and scoped to what is being viewed. Bridge-level test asserting a snapshot with N agents attaches none. Prerequisite for everything below.
- [ ] **M2 — Model: daemon identity and tab membership.** `AgentSession` grows a daemon identity and a structured `tab`; the overview and drill-in key by `(daemonId, agentId)`. Rust DTO recovers orchestration cwd (`map_tab`'s dropped `..`).
- [ ] **M3 — Overview screen and grouping.** Daemon group as the outer unit; orchestration / mode / standalone groups inside it; honest columns per the table above. View state lives in `App`, above `ControlDeck`.
- [ ] **M4 — Drill-in and back.** Navigate to one agent or one orchestration, attach there, return to the overview. Existing deck behaviour preserved for a drilled-in orchestration.
- [ ] **M5 — Empty, disconnected, incompatible and zero-agent states**, plus a genuine "connected, zero agents" fixture (closing the `state=empty` → disconnected mis-mapping at `desktop/src/data/fixture.ts:264-268`).
- [ ] **M6 — Honest fields and de-fabrication.** Surface `last_user_prompt` and `live_target` through `map_agent`; remove the fabricated `attempt` and `branch` from what live mode presents as fact. No protocol impact.
- [ ] **M7 — Last activity (daemon-side, severable).** `last_activity` added to `SessionSnapshot` as an additive optional field — no `PROTOCOL_VERSION` bump — surfaced through the DTO and rendered. Includes rule 12's cross-version manual test. Drop this milestone and the PRD becomes purely desktop-side.
- [ ] **M8 — Coverage.** Testing Library tests for grouping, every state, and drill-in navigation; Rust `dto.rs` tests pinning each newly surfaced field's frontend-facing shape; the M1 attach assertion. Per rule 4, and per the honest ceiling stated above.
- [ ] **M9 — Docs and changelog.** `docs/develop/desktop-gui.md` updated (landing screen, attach model, the states, the extended manual smoke check); changelog fragment via the `dot-ai-changelog-fragment` skill.

## Risks

- **Demand-driven attach is a behaviour change to the existing deck, not just new code.** Get it wrong and a drilled-in terminal is blank, or duplicates output on re-attach. The existing generation/replay machinery (`desktop/src/lib/terminalBuffer.ts`, tested at `terminalBuffer.test.ts`) is what protects this, and re-attach replays scrollback from the daemon — but this is the change most likely to break something that currently works.
- **Re-attach latency on every drill-in.** A full scrollback replay per attach is fine once and annoying when bouncing between agents. Mitigated by keeping a small MRU set attached; the risk is that the "right" set is picked by guess rather than measurement.
- **An honest screen may read as an empty screen.** Removing model, cost, tokens, context and duration leaves status, tool, prompt and membership. That is genuinely useful information, but it is a smaller card than the fixture's, and the fixture is what everyone has seen. The design has to make the remaining columns carry their weight rather than looking like a stripped-down version of something better.
- **Scope creep toward #742.** Introducing a daemon identity is cheap; building per-endpoint connection state, partial-failure handling and a keyed bridge map is #742's whole job. The line is: this PRD adds the *key* and the *outer group*, and nothing else.
- **Scope creep toward #633.** Every rejected column has an obvious "but we could just…" answer. Each of them means new daemon state, and that is a different PRD.
- **No end-to-end coverage of the real window.** The screen can be proven correct against a hand-built snapshot and proven not to attach terminals at the bridge, but nothing automated proves it works in the actual Tauri app against a real daemon. The manual smoke check is the only backstop, and manual checks decay.
- **PRD #176 M1.3 remains unmeasured.** This PRD reduces the load (no terminals on the landing screen) without measuring anything, so the ceiling stays unknown — better, but still unquantified.

## Open Questions

1. **Does drill-in target an agent or an orchestration?** Clicking a role inside an orchestration group could open just that agent, or open the whole orchestration deck focused on that role. The second matches how the TUI works and how people actually supervise, but it makes "drill into one agent" and "drill into a group" the same action with different focus. Leaning: orchestration groups open the orchestration with the clicked role focused; standalone agents open alone.
2. **Is the flat deck still reachable as a "show everything" view?** Today it is the only view. After this PRD, the overview is the entry point and drill-in is the way to a terminal — but a user with three agents may simply want the old deck. Leaning: no separate toggle initially; if it is missed, it is one view value away.
3. **Does the app remember where you were?** Relaunching always onto the overview is predictable and matches the PRD title; returning to the agent you were working in is kinder to someone mid-task. Leaning: always land on the overview, because "landing screen" is the point, and drill-in is one click.
4. **Is M7 (`last_activity` daemon-side) in this PRD or a follow-up?** It is the only milestone that touches the daemon and the only one that triggers rule 12's cross-version manual test. Including it makes the overview genuinely useful on first paint; deferring it keeps this PRD purely desktop-side and its rule 12 answer an unqualified no. Leaning: include, sequenced last so it can be dropped without disturbing M1–M6.
5. **Detach policy on leaving a drill-in.** Detach immediately (cheapest, replays on return), keep an MRU set (smoother, unbounded if unmanaged), or never detach within a session (fastest, re-creates today's problem slowly). Leaning: MRU with a small fixed bound, chosen by measurement during M1.
6. **How much does the single-daemon group header show?** Enough that a second daemon slots in without a redesign, little enough that it is not chrome around a single item. This is a design question the implementation should answer visually, not one to settle on paper.

## Work Log

### 2026-08-30 — Created

Written from the placeholder in [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) after a reconnaissance pass over `desktop/` and the daemon protocol. Three things the placeholder did not know, which changed the plan:

- **Terminal attach is eager and snapshot-driven, not mount-driven.** The issue's constraint "do not attach a terminal per agent on this screen" turned out to be unachievable by simply not rendering terminals — `attachAgents` fires on every snapshot regardless of what is on screen (`desktop/src/lib/bridge.ts:367-373,488,506`). Making attach demand-driven is therefore M1, a prerequisite rather than a design principle.
- **Almost nothing here needs a protocol bump.** `PROTOCOL_VERSION` is 8 and its own policy names additive optional fields as an explicit do-not-bump case (`src/daemon_protocol.rs:11-14`). Three of the fields the issue assumed were missing — write lease, last user prompt, orchestration cwd — are already on the wire and merely dropped by the desktop's own Rust DTO.
- **The `experimental` question was already answered.** PRD #176 decision 6 (`prds/176-desktop-gui.md:101`) records that the flag does not apply to the GUI binary at all, so rule 9's question is settled by precedent rather than re-decided here.

Also noted: `?fixture=1&state=empty` is not the "connected, zero agents" case — it routes through the disconnected branch (`desktop/src/data/fixture.ts:264-268`), so the first-run state currently has no fixture at all.
