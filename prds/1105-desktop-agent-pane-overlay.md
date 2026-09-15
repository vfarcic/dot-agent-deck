# PRD #1105: An agent pane overlay for the desktop

**Status**: Not started — this document is the plan, written before any implementation.
**Priority**: Medium
**Created**: 2026-09-15
**Issue**: [#1105](https://github.com/vfarcic/dot-agent-deck/issues/1105)

## Problem Statement

The desktop app can show you nine agents at tile size, and it cannot show you one agent's terminal large. `DeckSurface` renders one flat `.agent-grid` over every agent the selected deck owns, each tile mounting a terminal by default (`tabs[agent.id] ?? "terminal"`, `desktop/src/App.tsx:311`), and every tile is the same size as every other. Reading the one agent that matters right now means reading a ninth of the window.

Two partial escapes exist, and the shape of what is missing is exactly the gap between them.

**The Reader overlay is large and is not a terminal.** `OutputReader` renders an agent's output at reading width, re-snapshotting the resolved xterm buffer every 700ms (`desktop/src/components/OutputReader.tsx:8,37-45`). It is genuinely useful for reading, and it is a *text* projection: `terminalSnapshotText` resolves the buffer to plain text, so there is no cursor, no colour, no alternate screen, and no keyboard into the agent. It answers "what did this agent say" and cannot answer "work with this agent".

**The overview is an index with nowhere to go.** PRD [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) built a fleet overview that deliberately mounts no terminal and attaches no PTY, and it named this gap in its own **Staging** section rather than leaving it to be discovered: *"the overview only becomes the landing screen once there is somewhere to go from it: a group view … and a single-agent view. Until then, making it the default would land users somewhere they cannot leave."* That is its deferred **Iteration 3 — destinations**. #745 shipped iterations 1 and 2 and left iteration 3 open. The overview today tells you which agent you want and then hands you nothing.

**And the third rendering is already halfway written.** An agent pane in this app is not a small widget. `AgentTile` carries five panel tabs — terminal, diff, checks, handoffs, artifacts (`desktop/src/components/AgentTile.tsx:22-28`) — plus evidence, prompts, rename, status, an embedded `TerminalViewport` and the Reader launcher, behind a sixteen-field prop interface (`:30-49`). Building an overlay as a second component beside it, with `OutputReader` already being a third rendering of the same agent's output, is how one agent ends up with three surfaces that drift apart one bug fix at a time. That outcome — not the layout — is what makes this a PRD.

## Solution Overview

Clicking an agent opens **that agent's pane, enlarged, inside the app**, from either of two screens: an agent card on the overview, and a tile on the deck grid. Same overlay, same component, two entry points. `Esc` or a close control returns the user exactly where they were.

Four commitments define it.

**One component at two sizes.** The grid tile and the overlay are the *same* component, rendered at two sizes, and every difference between the sizes is a **prop**. If the implementation finds itself wanting a second component, the props are wrong and that is the signal to stop. This is the central commitment of the whole PRD: the outcome being avoided is three copies of an agent pane, and the test of success is that a fix to a status chip, a tab, or a keyboard handler lands once.

**Closing costs nothing.** Returning from the overlay must put the user back with no terminal flicker, no re-attach and no scrollback replay. That is not free by default — it is a property of the bridge's warm set, and it is only preserved by keeping the deck grid mounted underneath. See [What closing must cost, and why the grid stays mounted](#what-closing-must-cost-and-why-the-grid-stays-mounted).

**Enlarging really enlarges.** The overlay asks the daemon for a bigger PTY rather than scaling a small one up. That is a product decision with a consequence for other people watching the same agent, and it is taken deliberately — see [Resizing the PTY, and what a TUI user sees](#resizing-the-pty-and-what-a-tui-user-sees).

**In-app navigation, not a router.** `DeckView` (`desktop/src/types.ts:276-278`) is already a discriminated union built for exactly this, and its own doc comment says so — *"so PRD #745 iteration 3's group and single-agent views arrive as added variants rather than as a refactor of a boolean. No router library is warranted for this."* This adds a variant. It adds no dependency.

### What this takes from #745's iteration 3, and what it leaves

Stated explicitly, because the risk here is quietly redefining someone else's deferred milestone to be whatever happened to get built.

#745's iteration 3 is written as four things: **a group view** (the existing deck, filtered to one tab bucket), **a single-agent view**, **drill-in navigation between them**, and **promoting the overview to the actual landing screen** (`prds/745-desktop-agent-overview-landing-screen.md:282`).

This PRD takes the **single-agent view** and the **navigation to it from both screens**. It leaves the **group view** and the **promotion to landing screen** untaken, and neither is reduced in scope by being left — the group view is still the whole deck-filtered-to-a-bucket screen #745 describes, with its own unanswered question about how many terminals it mounts (#745 open question 1). **#745's iteration 3 is therefore still open after this ships**, and should not be closed by it.

## Scope

### In Scope

- **One agent-pane overlay component**, opened from both the overview's agent cards and the deck's tiles, rendering the same component the grid tile renders, at a second size, differing only by props.
- **A third `DeckView` variant** carrying the agent's composite identity and the screen it was opened from, so the back behaviour is a property of the view value rather than of a history stack.
- **The deck grid staying mounted and attached underneath the overlay**, which is what makes closing free.
- **A genuine PTY resize on open and on close**, with the daemon's existing smallest-viewport policy left exactly as it is.
- **Cross-deck opening by switching the selected deck first and reverting on close**, reusing the existing single-deck attach with no wire change.
- **A click target on the overview's agent cards**, and nothing else on those cards.
- **Keyboard**: `Esc` closes, matching `OutputReader`'s existing binding (`desktop/src/components/OutputReader.tsx:55`).
- **Test coverage** at the two tiers the desktop actually has — vitest/jsdom and Playwright over the built bundle — plus the shown-set assertions at bridge level.
- **A docs update** to `docs/develop/desktop-gui.md`, including the manual smoke check, since that check is the only thing that exercises the real window.

### Out of Scope

- **The group view**, and therefore deck → group → agent as a second path. Still #745 iteration 3's.
- **Promoting the overview to the landing screen.** The deck stays the default; `DeckShell`'s `initialView = { kind: "deck" }` (`desktop/src/App.tsx:120`) is unchanged.
- **Any change to what the overview's cards display.** #745's commitment — *"No terminal on this screen, ever"* — is untouched. The cards gain a click target and nothing else.
- **Cross-deck attach.** A tile's terminal is always the selected deck's. Making a non-selected deck's terminal attachable is [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073), which records why it is not a refactor: the attach frame header carries no stream id, so multiplexing is a wire change and a `PROTOCOL_VERSION` bump (`src/daemon_protocol.rs:649-666`).
- **Retiring `OutputReader`.** The Reader is a *reading* projection with reflowed soft-wrapped lines and a copy control; the overlay is a *working* surface. Whether the overlay eventually subsumes it is [an open question](#open-questions), not a scope item — but the overlay must not become a fourth thing while that is undecided.
- **The per-tile composer**, whose removal is [#1042](https://github.com/vfarcic/dot-agent-deck/issues/1042) and lands first on this same branch as its own PR. The overlay inherits whatever that settles; it does not re-open it.
- **Measuring the WebGL context ceiling.** #745 records it as unmeasured and this PRD does not measure it either — see [Attach cost and the WebGL question](#attach-cost-and-the-webgl-question) for what this work does and does not do about it.
- **The `experimental` feature flag.** Not applied — see [Feature flag](#feature-flag).
- **Any harness that drives the real Tauri window.** None exists ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)); building one is its own PRD.

## Technical Approach

### The component, and the thing being avoided

`AgentTile` is the component. It grows a size prop — one prop naming which of two presentations to render, not a pile of booleans — and the overlay renders `AgentTile` with that prop set. Everything the two sizes differ in (which panel tabs are visible, whether the header is compact, how the terminal's box is computed, whether the Reader launcher shows) is derived from that one prop inside the component.

The rule that makes this enforceable rather than aspirational: **if a difference cannot be expressed as a prop, the answer is not a second component — it is that the difference is wrong.** The counter-example to keep in view is `OutputReader`, which *is* a genuinely separate component and earns it by being a different projection of the data (resolved text, not a terminal). "Bigger" is not a different projection.

### Screen and view state

`DeckView` today is `{ kind: "deck" } | { kind: "overview" }` (`desktop/src/types.ts:276-278`), and `DeckShell` switches on it with an early return: `if (view.kind === "overview") return <AgentOverview …/>; return <DeckSurface …/>` (`desktop/src/App.tsx:137-138`). So the union as used today **replaces** the mounted screen, and `DeckShell`'s own doc comment states the consequence — *"The deck is unmounted while the overview is up"* (`:126-127`), written up as the reason the zoom keys once died on the overview.

That is the constraint the new variant has to be designed around, because an overlay that unmounts the screen beneath it forfeits the no-flicker property before any of the rest of this matters. So the variant carries its origin:

```ts
| { kind: "agent"; deckId: string; agentId: string; from: "deck" | "overview" }
```

and `DeckShell` renders the base screen named by `from` **with the overlay on top of it**, rather than instead of it. The deck stays mounted when you arrived from the deck; the overview stays mounted when you arrived from the overview.

**This answers #745's open question 2** — *"Two paths to the agent view. Once destinations exist, overview → agent and overview → group → agent reach the same screen with different back behaviour. Worth deciding rather than discovering."* The decision is that **back behaviour is carried in the view value**, not inferred from a history stack. Closing sets `view` to `{ kind: from }`. When the group view arrives, it adds a third `from` and nothing else changes. Note the narrower claim: this answers the question for the two paths that exist after this PRD, and defines the mechanism the third path will use; it does not build the third path.

`deckId` rather than a bare agent id because agent ids are per-daemon monotonic integers and are *"unique only within a daemon"* (`desktop/src/types.ts:314`). `AgentSession` already carries `daemonId` (`:473`) and the overview already keys by the composite (`:470-473`), so the identity this variant needs exists. **What does not exist is composite keying everywhere**: #745 recorded nine maps keyed by a bare id and fixed only some, and this PRD does not fix the rest — but one of them is load-bearing here and is dealt with under [the cross-deck path](#cross-deck-switch-and-revert).

### What closing must cost, and why the grid stays mounted

The bridge's warm set is what makes returning free, and its bounds are what make "just unmount the deck" wrong.

`setShownTerminals` is declarative and does four things in one pass — attach the newly shown, move the newly hidden into the warm set, evict warm overflow, and flush the warm set entirely when nothing is shown (`desktop/src/lib/bridge.ts:1647-1691`). Leaving a terminal does **not** detach it: it moves to the warm set, and *"It stays in `attached`, so coming back costs no attach and produces no replay — the whole point of warm"* (`:1667-1669`). But the warm set is bounded at three — `MAX_WARM_TERMINALS = 3` (`:1385`) — and the flush-to-zero case is unconditional: when the shown set is empty the overflow is `this.warm.size`, *"Flushed to ZERO rather than down to the bound"* (`:1675-1679`).

So the arithmetic for a nine-tile deck is not close. Unmount the grid under the overlay and the shown set becomes one id; the other eight move to warm; five are evicted immediately. Closing the overlay then re-attaches five agents, and an attach is not cheap — the daemon *"immediately follows the OK response with a single `KIND_STREAM_OUT` carrying the consistent scrollback snapshot"* before streaming (`src/daemon_protocol.rs:67-69`), over a socket opened per session (`desktop/src-tauri/src/terminal.rs`). Five scrollback replays is precisely the flicker this design exists to avoid.

**Decision: the deck grid stays mounted and attached underneath the overlay.** The shown set does not change when the overlay opens over the deck, so `setShownTerminals` is not called at all — the effect that calls it keys on the joined id list (`desktop/src/App.tsx:324,339-342`), which is unchanged. Zero attaches, zero detaches, zero replays, on open and on close. The steady-state cost of the overlay over the deck is therefore **exactly today's deck cost**, and the feature adds no sockets.

**The `setShownTerminals` contract is a hard constraint on how this is wired.** Its doc comment: *"It must be called ONCE per render commit with every shown id, never once per tile: nine single-id calls would leave eight of the nine warm and evict five of them, which is the same broken deck the bound exists to avoid"* (`desktop/src/lib/bridge.ts:1652-1656`). Exactly two non-test call sites declare a set today — `grep -rn setShownTerminals desktop/src --include='*.ts' --include='*.tsx'` returns the deck's (`desktop/src/App.tsx:339-342`, whose own comment restates the rule and notes that deleting the effect *"does not fail a bridge test"*), the overview's, which declares the empty set (`desktop/src/components/AgentOverview.tsx:610-612`), the type (`desktop/src/types.ts:790`), the hook passthrough (`desktop/src/hooks/useDeckRuntime.ts:223,260`) and the test files.

That second one is the trap in the `from: "overview"` path, and it is a trap by construction rather than by carelessness: the overview declares `[]` on mount while the overlay above it needs `[agentId]`, which is two declarations in one render commit and the exact pattern the contract forbids. **So the shown-set declaration must have a single owner that can see both the base screen and the overlay** — one call, one set, per commit. Getting this wrong does not fail loudly: depending on effect order it produces either an overlay with a dead terminal or an overview that has quietly attached one PTY while its own doc comment says it attaches none. M4 owns it and M8 pins it with a test.

**One xterm per agent, always.** The tile beneath the overlay must not also be rendering a live `TerminalViewport` for the same agent, because two viewports means two WebGL contexts and two `ResizeObserver`s both calling `resizeTerminal(agentId, …)` into a single per-agent coalesced entry (`desktop/src/lib/bridge.ts:2456-2461`) — so the one viewer geometry the daemon holds would ping-pong between tile size and overlay size every frame. The required property is **exactly one live xterm instance per agent at any moment**. Two mechanisms can deliver it and the choice is M3's:

1. **Promote the tile's own element in place** (fixed positioning over the grid, same React subtree). Keeps the xterm instance, adds no WebGL context, costs one `fit()`. Preferred.
2. **Re-parent into an overlay container.** Re-parenting remounts the DOM node xterm is bound to, so the instance is recreated and the transcript re-written client-side (`desktop/src/components/TerminalViewport.tsx:132-133`). No socket churn and no daemon replay, but a visible repaint on both open and close.

A third `TerminalViewport` mounted for an agent that already has one is rejected outright, for the reason above.

### Resizing the PTY, and what a TUI user sees

This is a product decision, recorded as one rather than left to fall out of the implementation.

A PTY has exactly one window size. The daemon negotiates it as **the smallest each axis over every registered viewer**, with the two axes minimised independently, matching tmux's `window-size smallest` (`src/agent_pty.rs:7187-7209`). Viewers are keyed per attach token rather than per client process, precisely because *"One process can hold two views of the same agent (two desktop tiles, or a tile plus the Reader overlay)"* (`src/agent_pty.rs:2084-2095`). The desktop's side of that contract is already written down in `TerminalViewport`: *"`fit()` PROPOSES a size; the daemon disposes"* — the tile measures its box, reports it as a request, then puts the grid back to whatever the daemon last applied (`desktop/src/components/TerminalViewport.tsx:138-172`).

**Decision: let it resize.** Opening the overlay raises this client's requested geometry; closing it lowers it back.

**What a TUI user watching the same agent observes.** Their pane reflows when the overlay opens, and reflows back when it closes. The direction is the safe one and is bounded: because the policy takes the minimum, raising the desktop's request can only raise the PTY *up to* the smallest of the other attached viewers and never past it, so a TUI user's pane is never asked to render a grid larger than itself. Nothing is truncated for them. What they see is output reflowing to a different width, twice per overlay open/close cycle, when the desktop was the constraining viewer. When they are the constraining viewer, they see nothing at all and the overlay renders at their grid with the remainder of its box unused — which is `applyAppliedGrid` working as designed (`desktop/src/components/TerminalViewport.tsx:153-166`), not a bug to file.

**That bound holds only for a client that registers a viewport, and the exception is worth knowing before the overlay makes it visible.** `effective_dims` minimises over viewers that have *actually reported a geometry* — *"Only viewers that have actually told us a geometry constrain anything"* (`src/agent_pty.rs:7202-7205`) — and returns `None`, meaning leave the size alone, when none has, which the doc comment says covers *"an agent whose only client is a legacy one that never registers a viewport"* (`src/agent_pty.rs:7190-7193`). So a viewer old enough not to register does not constrain the minimum, and a desktop overlay enlarging past its pane can hand it a grid larger than itself. That is PRD #882's pre-existing exposure rather than something this feature creates — any deck client growing its pane does the same — but the overlay is the largest single jump in requested geometry the app has, so it is the thing most likely to surface it. Not fixed here, and not claimed to be.

**The options, and why this one.** Three were available. *Let it resize* gives the user more content, which is the entire point of the feature, at the cost of reflow for other viewers. *Suppress the resize while the overlay is open* — keep reporting the tile's geometry — costs nothing to others and makes the feature a magnifying glass: the same eighty columns at a larger font, in a window that could hold two hundred. That is not worth building. *Scale the overlay to the applied grid and pad* is what the code already does whenever another client constrains it, so it is not a third option so much as the graceful degradation of the first, and it needs no new mechanism.

The trade is stated plainly rather than hidden: **this feature can reflow someone else's terminal.** PRD #882's policy is what makes that merely a reflow instead of a corruption, and the reason it is acceptable is that the smallest-wins rule was built for exactly this — multiple viewers of one PTY, each guaranteed to be on the padding side of a mismatch rather than the truncating side.

### Attach cost and the WebGL question

**What one attach costs**, concretely: one socket per session; an `attach-stream` request, to which the daemon replies OK and *"immediately follows … with a single `KIND_STREAM_OUT` carrying the consistent scrollback snapshot, then enters streaming mode"* (`src/daemon_protocol.rs:67-69`); and a daemon-side task feeding it. Client-side it costs an entry in each of the bridge's per-agent maps and a viewer registration that constrains the PTY's size.

**Steady state with the grid mounted underneath is unchanged from today.** The overlay adds no attach, because it shows an agent that is already shown; that is the whole argument for keeping the grid mounted, and it is why the overlay is cheap in the deck path. In the `from: "overview"` path the cost is one attach — the overview's shown set is empty, so opening an agent goes from zero attached to one, which is exactly #745's commitment that *"whatever is showing output is what attaches"* and is the minimum the feature can possibly cost.

**WebGL.** Every mounted `TerminalViewport` allocates an xterm with 8000 lines of scrollback (`desktop/src/components/TerminalViewport.tsx:81`) and attempts a WebGL context in a `try`, falling back to the DOM renderer on throw or on context loss (`:118-128`). #745 records that browsers cap concurrent contexts, that the cap is **unmeasured**, and that PRD #176's M1.3 throughput qualification is still outstanding. That remains true and **this PRD does not measure it**. What it does is avoid making it worse: the one-xterm-per-agent rule above means the overlay allocates **no additional context** over the deck path, and in the overview path it allocates one where the screen previously had none. So the ceiling is unchanged in the deck path and approached from zero in the overview path. Anyone who wants the number still has to go and get it; this document does not pretend it has been got.

### Cross-deck switch-and-revert

The overview merges every observed deck's agents into one screen (PRD [#742](https://github.com/vfarcic/dot-agent-deck/issues/742)), while *"a tile's terminal is always the SELECTED deck's"* (`desktop/src/components/AgentOverview.tsx:600-602`). So the overview can offer an agent this app cannot currently attach to.

**Decision: opening such an agent switches the selected deck to that agent's deck first, reuses the existing single-deck attach, and reverts the selection on close.** No wire change, no `PROTOCOL_VERSION` bump, no cross-deck attach.

**And this path is expensive, which the design records rather than glosses.** Switching decks is not a local state flip. The selector writes the settings document and nothing else (`desktop/src/components/DeckSelector.tsx:153-158`), deliberately — *"`desktop_set_settings` → `apply_selection` is what puts a selection into force, and it is one function precisely so that dropping the links, releasing the tunnels, telling the watcher and emitting a fresh snapshot cannot be done by halves"* (`:29-33`). That command **persists the document to disk** and then applies it (`desktop/src-tauri/src/lib.rs:1422,1428`), and the apply path, when the deck actually moved, calls `terminal::detach_all` (`:1513-1516`), which detaches every session (`desktop/src-tauri/src/terminal.rs:717-729`). `apply_selection`'s own doc comment says it outright: *"the switch half **detaches every terminal session**"* (`desktop/src-tauri/src/lib.rs:1440-1442`).

Three consequences follow, all of them true of the cross-deck path and none of them true of the same-deck path:

1. **The no-flicker guarantee does not extend here.** Opening detaches the original deck's terminals; closing detaches the second deck's and re-attaches the original's with a full scrollback replay each. The commitment in this PRD is therefore scoped: *closing the overlay costs nothing on the same-deck path, and costs a reconnect on the cross-deck path.* Claiming it for both would be false.
2. **The revert has to force a re-declaration, and will not get one for free.** The deck's shown set is derived from `snapshot.agents` and joined into a dependency key (`desktop/src/App.tsx:310-324`), and agent ids are per-deck monotonic integers starting at 1. Two decks each running agents `1`, `2`, `3` therefore produce the **same key** before and after the round trip, so the effect does not re-fire and the restored deck is left with tiles whose sessions the switch tore down. This is #745's bare-id keying showing up somewhere it bites, and M6 owns fixing it at this call site specifically — the other maps #745 left keyed by a bare id stay out of scope.
3. **A transient navigation writes persistent configuration, twice.** If the app is killed while the overlay is open, the user's saved deck selection is the one the overlay set. M6 must leave the document byte-identical after a clean round trip, and the residue after an unclean one is a known, accepted, documented cost.

These are the facts the decision was taken against, not an argument to re-take it. If the cost proves unacceptable in use, the fallback is to make cross-deck opening explicit rather than implicit — recorded as an [open question](#open-questions), not as a plan.

### Cross-version safety

Per CLAUDE.md rule 12, answered explicitly rather than as a formality, and verified against what the work actually touches rather than assumed from its shape.

**Every milestone in this PRD is TypeScript under `desktop/src/`.** The component work (M1–M3), the view variant (M2), the shown-set ownership (M4), both entry points (M5), the cross-deck round trip (M6) and the tests (M7–M9) are all frontend. The answer is therefore an unqualified **no**: no `PROTOCOL_VERSION` bump, no `.breaking.md` fragment, no cross-version manual test.

**The three things that would change that answer, named so that drifting into one is a decision rather than an accident.** First, anything that needs a field the DTO does not already carry would edit `desktop/src-tauri/src/dto.rs` — webview IPC, not the daemon wire, so still no protocol bump, but it puts the change into `cargo test-fast` and rule 2's clippy gate and it owes a `dto.rs` shape test in the style of `agent_mapping_is_frontend_stable`. Second, anything that needs a **new daemon verb** — and nothing here does; the resize path already exists end to end (`desktop/src-tauri/src/terminal.rs:685-690` → `resize_agent_as_viewer`) — would be a wire change and a bump. Third, **cross-deck attach** is a bump by construction (`src/daemon_protocol.rs:649-666`, no stream id in the frame header), which is one more reason it stays in #1073.

The switch-and-revert of M6 explicitly does **not** reach any of those: it drives `desktop_set_settings`, an existing command, with an existing document shape.

### Feature flag

CLAUDE.md rule 9 asks whether a new user-visible surface ships behind `experimental`. The answer is **no**, and it is a recorded answer rather than a fresh judgement. PRD #176 decision 6 settled it for the entire desktop binary (`prds/176-desktop-gui.md:101`): the flag is a presentation switch gating render and input seams inside the *TUI* binary, a separate GUI binary has no such seam because building and running it is itself the opt-in, and maturity is handled by packaging. #745's **Feature flag** section restates the mechanics and adds the confirming detail — the desktop crate links the root library so `features::experimental_enabled()` is callable, but `run()` never calls `init_and_watch`, so it would read the OFF default forever, and the desktop's only notion of a project directory is a development convenience with no good answer for a packaged app.

Nothing about this PRD reopens that. No wrapper function, no `graduate-` follow-up issue.

### Testing: what rule 4 means here

Rule 4 is written in the TUI's vocabulary — L1 (`insta` + `TestBackend`) and L2 (PTY + vt100, `e2e_*.rs`). This feature lands in the Tauri app, so the mapping is stated rather than assumed, following #745's section of the same name.

- **L1 equivalent: vitest + jsdom + Testing Library** (`pnpm test`, `desktop/package.json:14`), rendering the real components against a hand-built runtime. This is where the substance of this feature is provable: that both entry points produce the same component, that the shown set is declared once per commit with the right ids, that closing restores the originating view, and that the overlay mounts no second `TerminalViewport` for an agent that already has one.
- **Browser tier: Playwright over the built bundle** (`pnpm test:browser`, `desktop/e2e/*.spec.ts`, types via `pnpm test:browser:types`), driven through the fixture query strings in Chromium and WebKit with no daemon involved. This is where geometry and layout claims belong — that the overlay actually occupies the window, that the grid beneath is not unmounted, that `Esc` closes it.
- **The Rust half runs under `cargo test-fast`** if any is touched. Per [Cross-version safety](#cross-version-safety) the plan is that none is; if a DTO field turns out to be needed, that half arrives with a `dto.rs` shape test.
- **Nothing drives the real Tauri window.** There is no `tauri-driver` and no WebDriver session in this repository ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)), so real IPC, a real xterm over a real PTY, and the distribution's own WebKitGTK or WKWebView are exercised by nothing automated — `docs/develop/desktop-gui.md:943` says so in those terms. **This is not rule-4 parity and is not claimed as such.** The compensating control is the manual smoke check (`docs/develop/desktop-gui.md:1042`), which M10 extends to cover opening an agent from each screen, resizing, and closing back — and which, like every manual check, decays.

**The two things no automated tier here can prove**, stated so nobody reads green checks as covering them: that a real PTY genuinely resizes when the overlay opens, and what a TUI user attached to the same agent sees while it does. Both are manual, both are in M10's smoke check, and the second one needs two clients on one agent.

## Success Criteria

- An agent can be opened enlarged from the deck and from the overview, and both produce the same component — provable by the component's own identity in the test, not by inspection.
- Opening and closing the overlay over the deck causes **zero** attaches, **zero** detaches and **zero** scrollback replays, asserted at bridge level rather than observed.
- `setShownTerminals` is called exactly **once** per render commit in every overlay path, including the one where the base screen is the overview, with the whole shown set.
- The overlay renders more of the agent's output than the tile does — a genuinely larger grid — whenever no other attached client constrains it.
- Closing returns to the screen the user opened from, with the deck's scroll position, selection and panel state intact.
- The overview's cards still mount no terminal and the overview still attaches nothing until an agent is opened.
- Opening a non-selected deck's agent works, and closing it leaves the settings document byte-identical to what it was before.
- No new component renders an agent pane. The diff adds props to `AgentTile`; it does not add an `AgentTileLarge`.

## Milestones

- [ ] **M1 — Read the tile as a component.** Establish what actually differs between the two sizes before writing either: which of the five panel tabs belong in an overlay, what the header carries, what the terminal's box is, and whether the Reader launcher survives inside the overlay. Output is the prop, named and typed, plus the list of call sites that pass it. No behaviour change. This is the milestone that prevents the fork, so it lands before M2.
- [ ] **M2 — The view variant.** Add `{ kind: "agent"; deckId; agentId; from }` to `DeckView` (`desktop/src/types.ts:276-278`) and teach `DeckShell` to render the base screen named by `from` **with the overlay on top**, rather than instead of it (`desktop/src/App.tsx:137-138`). Closing sets `view` to `{ kind: from }`. Tests for both origins and for the back behaviour of each.
- [ ] **M3 — One xterm per agent.** Make the overlay render `AgentTile` at overlay size while guaranteeing the agent has exactly one live `TerminalViewport`. Pick between promoting the tile's element in place and re-parenting it, on the criterion in [What closing must cost](#what-closing-must-cost-and-why-the-grid-stays-mounted); record which and why in the Work Log. A test that the agent has one viewport, not two.
- [ ] **M4 — Shown-set ownership.** Move the shown-set declaration to a single owner that can see the base screen and the overlay together, so every path makes exactly one `setShownTerminals` call per commit with the whole set. The `from: "overview"` path is the one that breaks without this; the `from: "deck"` path must come out of it making no call at all, because its set does not change.
- [ ] **M5 — Both entry points.** A click target on the overview's agent cards (and nothing else added to them) and on the deck's tiles, each navigating to the variant with the right `from`. Keyboard-accessible; `Esc` closes.
- [ ] **M6 — Cross-deck switch and revert.** Switch the selected deck on open and revert on close, through `desktop_set_settings`. Force the re-declaration the revert does not get for free — per-deck monotonic agent ids make the shown-set dependency key identical across the round trip (`desktop/src/App.tsx:310-324`), so this call site is keyed by the composite as part of this milestone. Assert the settings document is byte-identical after a clean round trip.
- [ ] **M7 — Bridge-level coverage.** The assertion the whole design rests on: opening and closing the overlay over a nine-agent deck produces no attach, no detach and no eviction. Drive `TauriDeckBridge`, the bridge that owns PTYs, in the style of the M7 test PRD #745 added for the same reason.
- [ ] **M8 — Component and browser coverage.** vitest for the one-component claim, the two origins, the back behaviour, the single-viewport rule and the once-per-commit rule; Playwright over the built bundle for the geometry claims — the overlay occupies the window, the grid beneath is not unmounted, `Esc` closes.
- [ ] **M9 — Resize coverage, to the limit of what is automatable.** Assert that the overlay reports a larger geometry than the tile did, and that closing reports the tile's geometry back. What cannot be asserted here is that a real PTY moved; that is M10's.
- [ ] **M10 — Docs, smoke check and changelog.** `docs/develop/desktop-gui.md`: the overlay, the one-component rule, the attach model with the grid mounted underneath, the cross-deck cost, and the manual smoke check extended to opening from each screen, resizing, closing back, and — with a TUI attached to the same agent — watching the reflow. Changelog fragment via the `dot-ai-changelog-fragment` skill.

## Risks

- **The fork happens anyway.** The single most likely failure is that someone finds one difference that is awkward as a prop and writes `AgentTileOverlay` "for now". Everything else in this PRD survives being done imperfectly; this does not. M1 exists to make the differences explicit before either size is written, and the success criterion is stated as a property of the diff for the same reason.
- **The grid is unmounted under the overlay "to save resources".** It is the intuitive optimisation and it is backwards: unmounting a nine-tile deck evicts six terminals past `MAX_WARM_TERMINALS` and buys five scrollback replays on close. The saving is real and the cost is the exact thing the feature is for.
- **Two `setShownTerminals` calls in one commit, silently.** The overview declares `[]` on mount and an overlay above it needs one id. Nothing fails loudly: depending on effect ordering the result is a dead overlay terminal or an overview that has attached a PTY while claiming it attaches none. M4 and M8 are the whole defence.
- **A second `TerminalViewport` for an agent that already has one.** Two WebGL contexts and two `ResizeObserver`s writing one coalesced per-agent resize entry, so the daemon's viewer geometry oscillates. It would look like an intermittent reflow rather than like a wiring mistake, which is what makes it expensive to find later.
- **The cross-deck path is a different feature wearing the same button.** Same click, same overlay, and underneath it a settings write, a full detach, and a reconnect on the way back — while the same-deck path costs nothing. A user who has only ever opened same-deck agents will read the first cross-deck open as a hang. Whether it needs its own affordance is an open question below.
- **A crash while a cross-deck overlay is open leaves the selection moved.** The switch persists to `desktop.toml` before it applies. Accepted, documented, not defended against.
- **Reflowing someone else's terminal.** A TUI user watching the same agent sees their pane reflow when a desktop user opens an overlay, with nothing on their screen explaining why. Bounded by the smallest-wins policy so nothing is truncated for any viewer that registers a geometry — and not bounded at all for one that does not, which is PRD #882's pre-existing exposure that this feature is the most likely to surface. A genuine cross-client side effect, chosen knowingly.
- **The WebGL ceiling stays unmeasured.** This work does not raise the context count in the deck path and adds one in the overview path, and it measures nothing. #745 and PRD #176 M1.3 both still want the number.
- **The real window is still driven by nothing.** Every automated tier here runs in jsdom or a browser. The overlay's interaction with real WebKitGTK, a real PTY and real IPC is covered by a manual check, and manual checks decay.
- **Scope creep into the group view.** "While we are here, the deck filtered to one orchestration is nearly free" is how #745's iteration 3 gets half-built by a PRD that said it would not build it. The line is: this adds one view variant for one agent.

## Open Questions

1. **Does the overlay eventually replace `OutputReader`?** The Reader is a resolved-text projection with reflowed lines and a copy control (`desktop/src/components/OutputReader.tsx:19-23`); the overlay is a live terminal. They answer different questions today. Once the overlay exists, whether the Reader is a *mode* of it, a control inside it, or a fourth surface that should have been retired is worth deciding deliberately rather than by accumulation. Not decided here; deliberately not pre-empted either.
2. **Should cross-deck opening be explicit?** It costs a settings write, a full detach and a reconnect on both legs, while the same-deck path costs nothing behind an identical click. Options: leave it implicit and make it *look* slow honestly (a state on the overlay); confirm before switching; or disable the click for non-selected decks until #1073 lands. Leaning: implicit with honest feedback, revisited after the first real use.
3. **What does the overlay do when its agent disappears?** The daemon can end an agent while it is overlaid. Closing back to a deck that no longer has that tile is probably right, and "probably" is not a decision.
4. **Does the overlay get its own keyboard surface beyond `Esc`?** Next/previous agent without closing is the obvious ask, and it interacts with the shown set — stepping through nine agents one at a time is exactly the pattern the warm-set bound was sized for. Worth deciding before it is discovered.
5. **Carried forward from #745, and now answered — recorded here so it is not re-asked.** #745's open question 2 (two paths to the agent view, and the back behaviour each implies) is answered by the `from` field on the view variant: back behaviour is carried in the view value. #745's open question 3 (detach policy on leaving a terminal) was already settled in its own implementation — the warm set with `MAX_WARM_TERMINALS = 3` is the MRU-with-a-small-fixed-bound it was leaning toward — and this PRD depends on that answer rather than revisiting it. #745's open question 1 (does the group view mount a terminal per member) stays open, with the group view.

## Work Log

### 2026-09-15 — Created

Written from the constraints established by reading the desktop code directly, against seven decisions taken before drafting: in-app navigation via the existing `DeckView` union rather than a router; one overlay component opened from both screens; the tile and the overlay as the same component at two sizes; scope limited to the agent surface with the group view and the landing-screen promotion left in #745; cross-deck by switch-and-revert with no wire change; the overview's cards staying terminal-free; and no `experimental` flag.

**Three things the code said that the plan had to absorb rather than assume.**

*The view union replaces the screen.* `DeckShell` early-returns between `AgentOverview` and `DeckSurface` (`desktop/src/App.tsx:137-138`) and its doc comment states the consequence — *"The deck is unmounted while the overview is up"* (`:126-127`). A third variant written the same way would unmount the deck under the overlay, evict six of nine warm terminals, and buy five scrollback replays on close. So the variant carries `from` and `DeckShell` renders the base screen *with* the overlay rather than instead of it. This is the single structural constraint the rest of the design hangs off.

*The two hard parts are coupled, and the coupling runs the opposite way to intuition.* Keeping the grid mounted is what makes closing free; a mounted tile rendering its own `TerminalViewport` for the overlaid agent is what would make the resize incoherent, since the bridge holds one session and therefore one viewer token per agent (`desktop/src/lib/bridge.ts:2456-2489`) and the daemon minimises across viewers (`src/agent_pty.rs:7201-7209`). The resolution is "grid mounted, one xterm per agent" — both properties, at the cost of one rule that has to be enforced by a test rather than by structure.

*The cross-deck path is not the cheap reuse it reads as.* Decision 5 says "switch the selected deck first, reusing the existing single-deck attach", which sounds like a state flip. It is a settings-document write persisted to disk (`desktop/src-tauri/src/lib.rs:1422`) whose apply path calls `terminal::detach_all` when the deck moves (`:1513-1516`), and `apply_selection`'s own doc comment says the switch half *"detaches every terminal session"* (`:1440-1442`). The decision stands — it genuinely needs no wire change and no bump, which is what it was taken for — but the PRD scopes the no-flicker commitment to the same-deck path and states the cross-deck cost outright.

**One concrete defect the design has to fix rather than inherit.** The revert will not re-declare the shown set on its own. The deck derives its shown ids from `snapshot.agents` and joins them into the effect's dependency key (`desktop/src/App.tsx:310-324`), agent ids are per-daemon monotonic integers *"unique only within a daemon"* (`desktop/src/types.ts:314`), and two decks each running agents `1`,`2`,`3` therefore produce an identical key before and after the round trip — so the effect does not fire and the restored deck keeps tiles whose sessions the switch tore down. It is #745's bare-id keying biting at a specific call site, and M6 fixes that call site without taking on the rest.

**Written against first-hand evidence.** A parallel constraints survey was commissioned on the same five questions and had not landed when this was drafted, so every `file:line` here was established by reading the code directly rather than taken from it. Where the survey disagrees, the survey should be reconciled against this document and the difference recorded here — particularly on question 5, the revert cost, where this document's finding is that the revert is *not* free and the decision was taken assuming it was cheaper than it is.

**Not claimed, deliberately.** The WebGL context cap is still unmeasured and this PRD does not measure it. The real Tauri window is still driven by nothing automated. And this PRD takes one of #745's iteration 3's four parts, so #745's iteration 3 is still open after this ships.
