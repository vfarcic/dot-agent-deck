# PRD #745: An agent overview as the desktop app's landing screen

**Status**: In progress — iteration 1 (fixture only) complete
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

A fleet overview: every agent the daemon runs, grouped the way the daemon already groups them, described only by things that are actually true, with no terminal attached. Eventually it is what the app opens on, and you drill from it into an agent or an orchestration — but that promotion depends on those destinations existing, so it is staged deliberately rather than taken in one step (see [Staging](#staging)).

Three commitments define the screen.

**Honesty over completeness.** The overview shows what the daemon genuinely knows and nothing else. It is explicitly better to ship a narrow screen than a wide one padded with `Unavailable` — see [Columns: the central decision](#columns-the-central-decision), which settles this field by field. This extends to values the app currently *fabricates*: live mode renders `ATT 01` on every tile from a hardcoded `attempt: 1` (`desktop/src/lib/bridge.ts:163`, `desktop/src/components/AgentTile.tsx:135-138`), which is worse than showing nothing because it looks like data.

**No terminal on this screen, ever.** Whatever is showing output is what attaches. That requires making attach demand-driven, which is real work rather than a consequence of rendering no terminals — see [Demand-driven attach](#demand-driven-attach).

**Single-daemon today, daemon-shaped from day one.** The screen is written against one daemon, but the model carries a daemon identity and the layout's top-level unit is a daemon group — see [Preserving the multi-daemon extension](#preserving-the-multi-daemon-extension).

### Staging

The work is deliberately split so the *design* can be reviewed before any of the plumbing is built.

**Iteration 1 — fixture only.** The overview is built against the existing fixture transport, with no daemon involved, and ships **alongside** the current deck rather than replacing it: one rail button, deck still the default. It is knowingly a **dead-end dashboard** — informative, with nothing to click through to. The point is to settle layout, grouping, density and what a card carries while all of that is still cheap to change. This iteration touches no Rust and opens no socket.

**Iteration 2 — connected.** The same screen against a live daemon. This is where demand-driven attach becomes necessary (M7) and where the daemon-side `last_activity` field lands. Note the trap: because fixture mode opens no sockets at all, iteration 1 genuinely does not need the attach change — which makes it easy to carry the omission into iteration 2 unnoticed and ship a screen that is visually clean and costs exactly what today costs.

**Iteration 3 — destinations, and promotion to landing screen.** Deferred, explicitly. The overview only becomes the *landing* screen once there is somewhere to go from it: a group view (the current deck, filtered to one tab bucket) and a single-agent view. Until then, making it the default would land users somewhere they cannot leave. This is recorded here as the design's endpoint; whether it ships as a later iteration of this PRD or as its own is decided when we get there.

So this PRD as scoped **does not fully satisfy the title of issue #745**. It builds the overview and defers making it the landing screen. That is intentional and was the explicit decision — an overview with no exits is worse than no overview.

## Scope

### In Scope

- **The overview screen itself**: grouped agent cards, honest columns, live status, no terminals.
- **Grouping by the daemon's own tab buckets**: an orchestration reads as one unit with its roles in role order and its coordinator marked; mode tabs group by mode name; dashboard agents form a standalone bucket.
- **View state above `ControlDeck`**, as a discriminated union from the start even while it carries only two values, so later destinations arrive as added variants rather than as a refactor.
- **One "Overview" rail button** to reach it. Converting the rail into real navigation is not attempted here.
- **A daemon identity on the agent model** and composite `(daemonId, agentId)` keying, while there is still exactly one daemon and it costs nothing.
- **Fixture work**, which is where iteration 1's whole value sits: cutting the fixture's agent shape down to the columns that genuinely exist; a **crowded scenario** (`?fixture=1&state=crowded` — several orchestrations, a mode bucket, standalone agents), because a four-agent fixture cannot answer a question about many agents; and a genuine **connected-with-zero-agents** state, which has no fixture at all today.
- **Empty, unreachable and incompatible states.**
- **Demand-driven terminal attach** (iteration 2): attach where a terminal is actually shown, rather than for every agent in every snapshot.
- **Surfacing three fields already on the wire but dropped by the desktop's own Rust DTO** (iteration 2): write lease, last user prompt, orchestration cwd. No protocol change; see [Cross-version safety](#cross-version-safety).
- **Removing the two fabricated live-mode values** (`attempt`, `branch`) from what the app presents as fact.
- **`last_activity` as an additive optional field on `SessionSnapshot`** (iteration 2) — the one daemon-side change in this PRD.
- **The Linux build-prerequisite docs gap.** `docs/develop/desktop-gui.md:8` names no command and never says that `cargo test-fast` and rule 2's clippy gate *require* the Tauri system packages because both run `--workspace`. A Linux contributor hits `gobject-2.0 was not found` on their first commit attempt with no pointer to that page.

### Out of Scope

- **Model, token, cost and context-window columns.** They do not exist in daemon state at all — not on the wire, not in `RunningAgent` (`src/agent_pty.rs:1688-1846`), not in `SessionState` (`src/state.rs:670-707`). Creating them is [PRD #633](https://github.com/vfarcic/dot-agent-deck/issues/633)'s discovery work, not this screen's.
- **Git branch per agent.** Nothing tracks it daemon-side; the only `git branch` calls in `src/` are deletions in the dispatch flows. Reconstructing it means either a subprocess per agent cwd on the daemon, or a desktop-side git call that breaks the "local daemons only" boundary (`docs/develop/desktop-gui.md:97`).
- **Attempt/retry count.** No such counter exists anywhere. Removing the fabricated one is in scope; inventing a real one is not.
- **Session duration.** `SessionState.started_at` exists but is invented as `now` on hydration (`src/state.rs:5684-5694`), so any duration resets when the daemon restarts under a live agent. A duration that silently lies about long-running work is worse than no duration.
- **Terminal thumbnails, previews or output snippets on the overview.** Any of them re-creates the attach cost this PRD exists to remove. Worth noting for the future: this is the *only* thing discussed here that the daemon's API genuinely could not serve today — see [Why no new daemon API](#why-no-new-daemon-api).
- **Any change to the existing control deck's layout or contents.** It keeps working exactly as it does now and stays the default screen. Its eventual future is to become the group view (iteration 3), reached already filtered to one tab bucket; nothing here anticipates that beyond leaving it alone.
- **A group view and a single-agent view**, and therefore any drill-in navigation. Deferred to iteration 3 — see [Staging](#staging). This is what makes iteration 1 a dead-end dashboard, deliberately.
- **An unfiltered "show all agents" view.** Once the deck becomes the group view, a global all-agents variant is one more value in the view union. It is not built until someone misses it.
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

### Why no new daemon API

The natural reading of "the overview is cheap, the deck is expensive" is that the daemon over-serves and needs a leaner endpoint. It does not, and building one would solve a problem we do not have.

The daemon's API is already factored into exactly the two things needed, kept apart:

- **`ListAgents`** returns structured metadata per agent — `AgentRecord` (`src/agent_pty.rs:1958-2028`) with id, pane id, display name, cwd, tab membership, agent type, rows/cols, and a nested `live: Option<SessionSnapshot>` (`src/state.rs:639-668`) carrying status, active tool, tool count, first prompts, last user prompt and write lease. **That single response covers every column in the table above.** `get_snapshot` already builds the desktop's whole snapshot from that one call (`desktop/src-tauri/src/daemon_bridge.rs:196-221`).
- **`AttachStream`** is the PTY byte stream, expensive by nature: a full scrollback replay on connect, then live output.

Two other things ride alongside, both already in place and both **O(1) in agent count**: one global event subscription (`desktop/src-tauri/src/lib.rs:654`), which is what makes the screen live rather than frozen at connect, and the `Hello` handshake that produces the connected / disconnected / incompatible states.

So the overview costs **one RPC plus one already-open event stream, whatever the agent count**. Today's deck costs that *plus one `AttachStream` socket and one full scrollback replay per agent*. Nine agents: one connection instead of ten. The entire difference is client-side.

The one thing the daemon genuinely could not serve today is an *output preview* — last N lines per agent without a live subscription — because the only way to get output at all is to subscribe to all of it. Previews are out of scope precisely so that gap never has to be filled; if that decision is ever revisited, this is the daemon-side work it implies.

### Demand-driven attach

What makes the deck expensive is not that it renders terminals — it is that attach is not tied to rendering at all. `attachAgents` lives inside `TauriDeckBridge`, takes the snapshot payload, and attaches every agent it has not already attached (`desktop/src/lib/bridge.ts:367-373`), filtered on `this.attached` rather than on anything mounted. It is called from `connect()` (`:488`) and from the `desktop://snapshot` listener (`:505`), neither of which knows what is on screen. `TerminalViewport` triggers nothing.

The consequence is the thing most likely to be lost: **a screen that displays no output still opens a socket per agent.** Launch the app on an overview today and the bridge opens nine `AttachStream` sockets with nine scrollback replays, streaming bytes into in-memory buffers that nothing displays. "Shows no terminals" and "opens no PTYs" are separate properties, and only the first comes free.

So attach moves out of the snapshot listener and to wherever a terminal is actually shown. Detach on leaving is a judgement call — re-attaching replays scrollback so nothing is lost, but bouncing between two agents pays that replay each time; a small most-recently-used set is the likely answer, and measuring beats guessing.

Note this change is **only meaningful alongside a screen that shows agents without their output**. Shipped against today's deck alone it would be a no-op, since every agent is displayed with a terminal and "attach what is shown" and "attach everything" name the same set. That is why it belongs in this PRD rather than in one of its own.

All of it is desktop-side, and directly testable at the bridge level today: `bridge.test.ts:118-142` already asserts on attach behaviour against a mocked `invoke`, so "the overview attaches nothing" is a real assertion rather than an aspiration.

### Grouping

The daemon's `TabMembership` is the grouping key, exactly as the issue suggests:

- **Orchestration** — one card per orchestration, identified by `orchestration_id`, titled by display title or orchestration name, listing its roles in `role_index` order with the start role marked. This is the unit; its members are rows inside it, never peers of it.
- **Mode** — grouped by mode name.
- **Dashboard** — a standalone bucket for agents belonging to neither.

`AgentSession` grows a structured `tab` field so the frontend stops reconstructing membership from a role string. Everything needed is already in `DesktopAgentDto.tab`.

### Screen and view state

**The rail looks like navigation and is not.** Of its six buttons (`desktop/src/App.tsx:299-306`), five open overlay sheets over the always-mounted deck — `projectsOpen`, `promptsOpen`, `workflowOpen`, `profilesOpen`, and Settings, which only sets a notice — and the sixth, "Runs", is a no-op whose `onClick` closes all four panels and whose `active` is computed as "none of them are open" (`:301`). Nothing in the app is a view; every "which surface" decision is a boolean in `ControlDeck`'s own `useState` block (`:69-87`). So adding real view state fills in an intent the code already gestures at rather than fighting an existing model.

The overview renders *instead of* the deck, so the new state belongs in `App` (`:63-65`), above `ControlDeck`. It goes in as a **discriminated union from the start**, even while it carries only `deck` and `overview`, so that iteration 3's group and agent views arrive as added variants rather than as a refactor of a boolean. No router library is warranted.

The rail gains one **Overview** button. "Runs" keeps its current behaviour for now, even though it will eventually have no referent once the deck becomes the group view — converting the rail into real navigation is iteration 3's problem, and pre-empting it here would be guessing.

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

- An overview of every agent the daemon runs is one click from launch, and reading it does not require opening a terminal.
- Rendering the overview attaches **zero** PTYs, proven by an automated test rather than by inspection.
- Nine agents across two orchestrations read as two units, with roles in role order and each coordinator identifiable — not as nine unrelated tiles.
- Every value on the screen is one the daemon actually reported. Nothing is fabricated, and nothing reads `Unavailable`.
- A daemon that is down, incompatible, or owns zero agents each produce a screen that says what happened and what to do next — and each is reachable in the fixture without a real daemon.
- The crowded fixture scenario is legible: the design answers "what is running right now" at fifteen agents, not only at four.
- The existing control deck is unchanged in behaviour and remains the default screen.
- Adding a second daemon later means adding a sibling group and no inner component changes; nothing in the overview is keyed by a bare agent id.

## Milestones

### Iteration 1 — fixture only (no daemon, no Rust)

- [x] **M1 — Honest fixture.** Cut the fixture's agent shape down to the columns that genuinely exist, so the design is not settled against data (`model`, `cost`, `tokens`, `contextPercent`, `branch`, `spend`) that vanishes the moment it meets a real daemon. This is the trap iteration 1 exists to avoid, not a tidy-up.
- [x] **M2 — Crowded and zero-agent fixture states.** `?fixture=1&state=crowded` with several orchestrations, a mode bucket and standalone agents; and a genuine connected-with-zero-agents state, closing the `state=empty` → disconnected mis-mapping (`desktop/src/data/fixture.ts:264-268`) that currently makes the first-run screen impossible to view.
- [x] **M3 — View state and rail entry.** A discriminated view union in `App` above `ControlDeck` (`desktop/src/App.tsx:63-65`), plus one Overview rail button. The deck stays the default.
- [x] **M4 — The overview screen.** Daemon group as the outer unit; orchestration / mode / standalone groups inside it; honest columns per the table above. No terminals.
- [x] **M5 — Model: daemon identity and tab membership.** `AgentSession` grows a daemon identity and a structured `tab`; the overview keys by `(daemonId, agentId)`.
- [x] **M6 — Iteration 1 coverage.** Testing Library tests for grouping, the crowded scenario, and every connection state.

### Iteration 2 — connected

- [ ] **M7 — Demand-driven attach.** `attachAgents` stops firing for every agent on `connect()` and on every snapshot; attach happens where a terminal is shown. Bridge-level test asserting a snapshot with N agents attaches none while the overview is up. **Removing iteration 1's live-mode gate is part of this milestone's definition of done**: `showOverview()` in `desktop/src/App.tsx` currently hides the Overview rail button and refuses an `overview` view state outside fixture mode, precisely because attach is still eager. The gate must not outlive the reason for it — `grep showOverview` finds both seams and the test that pins them.
- [ ] **M8 — Honest fields and de-fabrication.** Surface `last_user_prompt`, `live_target` and orchestration cwd through `map_agent` / `map_tab` (`desktop/src-tauri/src/dto.rs:249-302`); remove the fabricated `attempt` and `branch` from what live mode presents as fact. Rust `dto.rs` tests pinning each newly surfaced field's frontend-facing shape. No protocol impact.
- [ ] **M9 — Last activity.** `last_activity` added to `SessionSnapshot` as an additive optional field — no `PROTOCOL_VERSION` bump — surfaced through the DTO and rendered. The only daemon-side change in this PRD; includes rule 12's cross-version manual test.
- [ ] **M10 — Docs and changelog.** `docs/develop/desktop-gui.md` updated: the overview screen, the attach model, the states, the extended manual smoke check, and the Linux build prerequisites gap — the literal five-package apt line from `.github/workflows/ci.yml:171-175`, the fact that `cargo test-fast` and rule 2's clippy gate need it because both run `--workspace`, and the devbox/nix `LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu` note for contributors working inside this repo's shell. Changelog fragment via the `dot-ai-changelog-fragment` skill.

### Iteration 3 — destinations (deferred)

Not built here, recorded so the design's endpoint is not lost: a **group view** (the existing deck, filtered to one tab bucket), a **single-agent view**, drill-in navigation between them, promoting the overview to the actual landing screen, and whatever the rail should become once it is real navigation. Whether this is a later iteration of this PRD or its own is decided when we get there.

## Risks

- **"The overview shows no output" is not "the overview opens no PTYs".** These come apart because attach is snapshot-driven, not render-driven, and conflating them is how M7 gets dropped as unnecessary and the whole thing ships as a cosmetic change. Iteration 1 genuinely does not need it — fixture mode opens no sockets — which makes the omission easy to carry into iteration 2 unnoticed.
- **Demand-driven attach is a behaviour change to code that currently works.** Get it wrong and a terminal is blank, or duplicates output on re-attach. The existing generation/replay machinery (`desktop/src/lib/terminalBuffer.ts`, tested at `terminalBuffer.test.ts`) is the protection, and re-attach replays scrollback from the daemon — but this is the change most likely to break something people rely on.
- **Designing against a fixture that lies.** The fixture carries `model`, `cost`, `tokens`, `contextPercent`, `branch`, `spend: 2.57` and `run_7f24a`, none of which exist in live mode. Approve a layout built on those and it goes half-empty on first contact with a daemon. M1 exists solely to remove this risk, and it has to land before the design is reviewed, not after.
- **A four-agent fixture cannot answer a many-agent question.** The premise of the whole PRD is that the current screen fails at scale; reviewing the replacement at four agents reviews the wrong thing. Hence M2.
- **An honest screen may read as an empty screen.** Removing model, cost, tokens, context and duration leaves status, tool, prompt and membership. That is genuinely useful, but it is a smaller card than the fixture's, and the fixture is what everyone has seen. The design has to make the remaining columns carry their weight rather than looking like a stripped-down version of something better.
- **A dead-end dashboard is only acceptable while it is additive.** Iteration 1 ships a screen you cannot navigate out of, which is fine only because the deck stays the default and nobody's workflow depends on the overview. Promote it to landing screen before iteration 3 exists and the app gets strictly worse than it is today.
- **Silent WebGL degradation, in the deck this PRD does not touch.** Every visible tile allocates an xterm with 8000-line scrollback and attempts a WebGL context (`desktop/src/components/TerminalViewport.tsx:61,93-104`); browsers cap concurrent contexts and past the cap the addon's `catch` drops that pane to DOM rendering with no indication. Nothing measures where that cap is. The overview sidesteps it; the deck and any future group view do not.
- **Scope creep toward #742.** Introducing a daemon identity is cheap; per-endpoint connection state, partial-failure handling and a keyed bridge map are #742's whole job. The line is: this PRD adds the *key* and the *outer group*, nothing else.
- **Scope creep toward #633.** Every rejected column has an obvious "but we could just…" answer. Each of them means new daemon state, and that is a different PRD.
- **No end-to-end coverage of the real window.** The screen can be proven correct against a hand-built snapshot and proven not to attach at the bridge, but nothing automated proves it works in the actual Tauri app against a real daemon. The manual smoke check is the only backstop, and manual checks decay.
- **PRD #176 M1.3 remains unmeasured.** This PRD reduces load without measuring anything, so the ceiling stays unknown — better, but still unquantified.

## Open Questions

1. **Does the group view mount a terminal per member, or only the focused one?** Deferred with iteration 3, but recorded now because it decides whether the group view answers the original complaint or relocates it: a single orchestration is commonly six roles, and the dashboard bucket is unbounded. Leaning: focused-only, with the rest as status cards — which makes the group view structurally the same component as the overview, just scoped.
2. **Two paths to the agent view.** Once destinations exist, overview → agent and overview → group → agent reach the same screen with different back behaviour. Worth deciding rather than discovering.
3. **Detach policy on leaving a terminal.** Detach immediately (cheapest, replays on return), keep a small MRU set (smoother, unbounded if unmanaged), or never detach within a session (fastest, re-creates today's problem slowly). Leaning: MRU with a small fixed bound, chosen by measurement during M7.
4. **How much does the single-daemon group header show?** Enough that a second daemon slots in without a redesign, little enough that it is not chrome around a single item. A design question to answer visually in iteration 1, not on paper.
5. **Should the crowded scenario eventually become the default fixture?** It is opt-in for now, because changing the default would churn the 14 existing `App.test.tsx` tests for no gain. If the crowded case is what people actually want to look at, that trade is worth revisiting once.

## Work Log

### 2026-08-30 — Created

Written from the placeholder in [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) after a reconnaissance pass over `desktop/` and the daemon protocol. Three things the placeholder did not know, which changed the plan:

- **Terminal attach is eager and snapshot-driven, not mount-driven.** The issue's constraint "do not attach a terminal per agent on this screen" turned out to be unachievable by simply not rendering terminals — `attachAgents` fires on every snapshot regardless of what is on screen (`desktop/src/lib/bridge.ts:367-373,488,506`). Making attach demand-driven is therefore real work rather than a consequence of rendering no terminals. (It was M1 when this entry was written; the scope review below moved it to **M7**, in iteration 2, because fixture mode opens no sockets and iteration 1 does not need it.)
- **Almost nothing here needs a protocol bump.** `PROTOCOL_VERSION` is 8 and its own policy names additive optional fields as an explicit do-not-bump case (`src/daemon_protocol.rs:11-14`). Three of the fields the issue assumed were missing — write lease, last user prompt, orchestration cwd — are already on the wire and merely dropped by the desktop's own Rust DTO.
- **The `experimental` question was already answered.** PRD #176 decision 6 (`prds/176-desktop-gui.md:101`) records that the flag does not apply to the GUI binary at all, so rule 9's question is settled by precedent rather than re-decided here.

Also noted: `?fixture=1&state=empty` is not the "connected, zero agents" case — it routes through the disconnected branch (`desktop/src/data/fixture.ts:264-268`), so the first-run state currently has no fixture at all.

### 2026-08-30 — Scope settled with the user

Design review with the user changed the shape of the work substantially. Recorded because most of it was arrived at by disagreement rather than by plan.

**Settled:** no `experimental` flag (rule 9, following PRD #176 decision 6). `last_activity` stays in this PRD rather than becoming a follow-up. The Linux build-prerequisite docs gap is folded in. The crowded fixture is opt-in (`state=crowded`) rather than the default, so the 14 existing `App.test.tsx` tests stay stable.

**The existing deck is preserved untouched**, and becomes the group view later rather than being replaced or dropped. This resolved a three-way question — delete it, keep it behind a "show all" toggle, or keep it as the drill-in target for a bucket — in favour of the third, which is better than any of the options originally offered: the deck reached already filtered to one tab bucket is the coherent unit, rather than an escape hatch back to the problem. An unfiltered all-agents view is deferred until someone misses it.

**Split into iterations, with iteration 1 built entirely against the fixture** so the design can be reviewed before any plumbing exists. Two traps were identified in doing so and are now milestones in their own right: the fixture *lies* (it carries model, cost, tokens, context and branch, none of which exist live), so a design approved against it would go half-empty on first contact with a daemon; and the fixture has only four agents, so it cannot answer a question whose entire premise is many agents.

**Navigation deferred, and the overview does not become the landing screen in this PRD.** Iteration 1 is knowingly a dead-end dashboard, acceptable only because it is additive — the deck stays default. This means the PRD as scoped does not fully satisfy issue #745's title, which is stated plainly above rather than glossed.

**A new daemon API was considered and rejected.** The question was whether the daemon should grow an endpoint returning "only what the overview needs". It should not: `ListAgents` already returns exactly that, in one call, and the expense is a *separate* per-agent `AttachStream` the desktop opens unconditionally. The daemon's API is correctly factored; the client conflates the two halves. Recorded in [Why no new daemon API](#why-no-new-daemon-api), along with the one case that would genuinely need daemon work — output previews — which is why previews are out of scope.

**Demand-driven attach stays in this PRD rather than becoming its own.** On its own it would be unobservable: today's deck displays every agent with a terminal, so "attach what is shown" and "attach everything" name the same set. It only becomes meaningful next to a screen that shows agents without their output.

### 2026-08-30 — Iteration 1 built (M1–M6)

The overview ships alongside the deck behind one rail button, against the fixture only. No Rust changed, `desktop/src-tauri/` was not touched, `attachAgents` was not touched, and nothing on the screen navigates anywhere — the dead end is intact.

**M1 was decided as annotation rather than amputation, and the constraint that decided it was the deck.** The dishonest fields are not removable without editing the control deck: `AgentTile` renders `model`, `attempt`, `duration`, `tokens`, `cost` and `contextPercent` directly (`desktop/src/components/AgentTile.tsx:121,135-138,146-160`), and the topbar renders `spend`, `branch` and `runId`. Deleting them from `AgentSession` would therefore have meant changing the deck's contents, which this iteration is explicitly forbidden to do, and would have churned the existing suite. So they stay on the type, and honesty is enforced *structurally* instead of by discipline: every `AgentSession` **data** field carries an `HONEST` or `FIXTURE-ONLY` annotation saying whether a daemon reports it (the deck-internal collections — `transcript`, `diff`, `checks`, `handoffIds`, `artifacts` — are annotated as one group, since live mode leaves all five empty), and `AgentOverview` renders from `OverviewAgent` — a `Pick<>` of the honest subset — so reaching for `model` or `cost` on the overview is a **compile error**, not a review catch. The overview's own test asserts the string `Unavailable` never reaches the screen.

**The crowded fixture carries live mode's placeholders verbatim rather than plausible numbers.** `crowdedAgent` sets `model: "Unavailable"`, `attempt: 1`, `duration: "—"`, `tokens/cost/contextPercent: 0`, `worktree: "Unavailable"` and `transcript: ""` — the exact values `agentFromDto` hardcodes. That makes `?fixture=1&state=crowded` a faithful preview of a real daemon at fifteen agents on *both* screens instead of a demo that flatters the design, which is the risk M1 exists to remove. Agent ids are `"1"`–`"15"` because that is what the daemon mints, and the fifteen are declared deliberately out of role order with `dot-ai`'s start role at `roleIndex` 2 rather than 0, so grouping, ordering and coordinator identification are three separate things the screen has to get right rather than one accident of declaration order. Statuses are restricted to `running`, `waiting` and `failed` — the only three `statusFromDaemon` can produce.

**`state=empty` now means connected-with-zero-agents.** Nothing depended on the old mis-mapping: no test referenced `createFixtureSnapshot("empty")`'s connection at all, and the two callers in `useDeckRuntime.ts:19,38` replace the whole `connection` object with a `loading` one before it is rendered. `health` deliberately stays `idle` for that state so the live-mode seed's honest health is unchanged.

**The overview's shape.** Daemon group as the outer unit (lamp, socket path, status, per-status pips) with the tab-bucket groups nested inside it, one column legend for the whole fleet, and one 34px row per agent on a grid the legend and every card share so fifteen rows read as one table rather than four unrelated lists. An orchestration is a card with a teal left edge, a `01`…`06` role index down its rows, and its coordinator badged wherever in the order it falls. Two per-group decisions came out of reading the rendered result at fifteen agents: the role name is shown only for orchestration members, because outside one `role` is derived from the agent type and merely restates the CLI column; and the working directory **most** of a group's members share is stated once in that group's header and left blank in those rows, which turns the column into a *differences* column — what a row prints is what makes it unlike its neighbours. In the crowded fixture the standalone `pi-extension` pane is the one row that prints one, because the other two standalone agents sit in the deck checkout that the header now states. (Corrected 2026-08-30: the first implementation hoisted only a value the *whole* group shared, so the standalone bucket — three agents across two directories — hoisted nothing and printed all three, two of them identical. The claim in this paragraph described the intent rather than the code; the code is now what the claim says, guarded per row by `states the working directory most of a group shares…` and by `hoists a working directory only when at least two members share it`.)

A note for whoever touches the deck next: `.coordinator-badge` has **no CSS rule anywhere in the repo**, despite `AgentTile.tsx:101` using it — the deck has been rendering an unstyled `COORDINATOR` span for as long as that line has existed. The overview's rule is scoped to `.overview-row .coordinator-badge` on purpose, so it does not restyle the deck; styling the deck's badge is a separate change this iteration is not allowed to make.

**Open Question 4 answered visually**: the single-daemon group header is a lamp, the socket path, the daemon's own status message and the status pips — enough that a second daemon slots in as a sibling, little enough that it is not chrome around a single item.

**Scope held.** The composite `(daemonId, agentId)` key exists only in the overview; the nine bare-`agentId` maps in the bridge, the deck and the terminal registry are untouched and remain #742's. `DesktopAgentDto.tab` now reuses the model's `AgentTab` rather than duplicating the shape, with a comment naming where a mapping function goes if the IPC shape ever diverges.

**One deliberate omission worth flagging for iteration 2**: the overview's disconnected and incompatible states offer Reconnect and a route to the deck, but not Start daemon or Replace daemon. Both of those are confirmation-gated on the deck (`desktop/src/App.tsx`), and duplicating that machinery onto a fixture-only screen was not worth it. If the overview is ever promoted toward the landing screen, it needs them.

Coverage: 20 new frontend tests (16 in `desktop/src/components/AgentOverview.test.tsx` — 13 `AgentOverview`, 2 `groupAgents`, 1 `DeckShell` — and 4 in `desktop/src/lib/bridge.test.ts`), taking the suite from 46 to 66. The one that matters most asserts the overview mounts **zero** terminals — spying on the mocked `TerminalViewport` so a mount-then-unmount still trips it — because that is the property the whole design rests on. Per the PRD's own testing note, there is no L2 equivalent for the desktop and none was invented.

### 2026-08-30 — Iteration 1, round 2: the security audit and the code review

An audit and a review ran over the iteration-1 diff. Every finding below was taken; the constraints were unchanged, so nothing here touches `attachAgents`, `desktop/src-tauri/`, Rust, the daemon, or navigation.

**The security work is all at one seam, and that is the point.** Daemon-supplied strings — display names, working directories, orchestration and role names, tool names and details — originate in agent processes, so they are attacker-influenced by prompt injection from any file an agent reads. The daemon-side scrub does not cover it: `src/daemon_client.rs:258-267` calls `strip_control_chars`, whose `char::is_control` test is general category `Cc`, while the bidi formatting codepoints are `Cf` — so a `U+202E` in a display name reaches the webview intact and visually reverses that name, swallowing the inline siblings printed after it, COORDINATOR badge included, on a screen whose entire purpose is telling one agent from another. (That daemon-side gap affects the TUI too and is filed separately.)

So `desktop/src/lib/displayText.ts` is a **display-only** sanitiser applied in `AgentOverview.tsx`, mirroring `src/untrusted_text.rs::strip_control_and_bidi` codepoint for codepoint: C0/C1 controls plus `U+202A`–`U+202E`, `U+2066`–`U+2069`, `U+200E`, `U+200F`, `U+061C`, enumerated rather than approximated. It is in TypeScript at the render seam rather than in the Rust DTO **because grouping, sorting and the composite `(daemonId, agentId)` key keep the raw values** — sanitising upstream would corrupt the keys and could merge two agents that differ only in a stripped character. Rendered text and `title` attributes go through it alike.

**Zero-width joiners are deliberately NOT stripped**, and the decision is recorded rather than defaulted. They cannot reorder or reverse text, so they do not produce the spoof the filter exists to stop; they are load-bearing in legitimate names (ZWJ builds emoji sequences, ZWNJ changes the word in Persian, Arabic and several Indic scripts); and widening past the Rust policy would make the TUI and the desktop disagree about the same daemon string, which is the divergence `untrusted_text.rs` was written to end. The residual — a name of entirely invisible characters rendering as a blank cell — is bounded by the length clamps and by the row still carrying status, CLI and tool columns. A test pins the decision so a later "tighten this" does not quietly break emoji.

**Every rendered string is bounded before React sees it**, because `DesktopAgentDto` is a TypeScript *assertion* about a shape the daemon supplies and not a validated one. Budgets, in characters: name 128 (the daemon's own `DISPLAY_NAME_MAX_LEN`, so any name it would accept passes through untouched), path 120, tool name 32, **tool detail 60**, `title` 512, connection message 240. The tool detail is short on purpose: for Bash/shell events it is the tool's first *command line* (`src/hook.rs:182-232`), which is both a disclosure risk in screenshots and unreadable at fifteen rows. It is **bounded, not redacted** — the audit's secret-scrubbing suggestion was declined, because a pattern heuristic buys false assurance where a length bound is honest. No virtualisation or row cap was added; that stays out of scope.

**Paths render home-relative and daemons get a short label.** A leading home directory becomes `~`, and the daemon group header shows the socket path's last segment instead of the whole path — both keep a username or uid out of a screenshot, and both keep the full value in the `title` and the raw value as the identity key. The webview has no `$HOME` (reading one would mean a `src-tauri` change), so the abbreviation matches the *shape* of a home directory — `/home/<user>`, `/Users/<user>`, `/root`, `C:\Users\<user>` — rather than the actual one. The crowded fixture's two hardcoded paths are now neutral (`/home/dev/...`): they stay home-*shaped* on purpose, so a fixture capture exercises the abbreviation in exactly the recordings where an absolute home path would be the thing worth hiding.

**The cwd column is now the differences column the Work Log already claimed it was.** See the correction in the entry above: the first implementation hoisted a value only when the *whole* group shared it, so the standalone bucket printed all three rows, two of them identical. It now hoists the **most common** value, and only when at least two members share it — so a group whose members all differ prints every row, and a group of one prints its own without the header repeating it. **This changes what the crowded fixture looks like**: the standalone card gains `~/code/dot-agent-deck` in its header, ids 13 and 14 go blank, and `pi-extension` becomes the only printed directory on the card.

**The screen is now a real table.** The column legend was `aria-hidden` and the rows were a CSS-grid `<ul>`/`<li>`, so no cell had a column header. Each group card is a `<table>` with `<th scope="col">` per column, the visible fleet-level legend stays decoration, and each table's own `<thead>` is present for a screen reader and hidden by `clip-path`. The layout is untouched because `display: grid` stays on the rows — which is precisely why the ARIA roles are **also** restated explicitly (`role="table"`/`"rowgroup"`/`"row"`/`"columnheader"`/`"cell"`): applying `display: grid` to table elements strips their implicit table semantics in browsers, so the honest markup alone would not have delivered the association it was chosen for.

Four smaller review findings: `writeLease` is now marked `FIXTURE-ONLY (until M8 surfaces live_target)` — it is a real data field hardcoded to `"unknown"`, and unmarked it read as honest, which is the failure the annotation scheme exists to prevent; the five deck-internal collections are annotated as a group and the "every field" claim above is scoped to data fields to match. `role` left `OverviewAgent`'s `Pick<>` — the row renders `displayName` and `tab.roleName`, so the honest projection is now exactly what the screen consumes. `groupAgents` no longer falls back to the orchestration *name* when `orchestrationId` is absent, which could merge two distinct orchestrations into one card with colliding role indexes; an id-less agent now gets a key unique to itself. And the `loading` connection state — a fifth real branch — has a test.

Coverage: 22 new frontend tests (9 in `desktop/src/components/AgentOverview.test.tsx`, 13 in the new `desktop/src/lib/displayText.test.ts`), taking the suite from 66 to **88**. The Rust count is unchanged at 3472, as it must be — no Rust was touched.
