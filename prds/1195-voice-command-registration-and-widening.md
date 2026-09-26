# PRD #1195: Make registering a voice command unforgettable, then widen the set

**Status**: In progress — written 2026-09-26 from issue #1195's body, on branch `agent/dispatch-issue-1195`.
**Priority**: Medium
**Created**: 2026-09-26
**Depends on**: [PRD #802](done/802-desktop-voice-control.md) (desktop voice control, shipped — the command table, `VOICE_ACTIONS`, and linkage-check rule 13 all come from it).

## Problem Statement

PRD #802 shipped desktop voice control with a deliberately narrow vocabulary: navigation only, no destructive actions, so that a misfire in the first slice could not be expensive. So a lot of obviously useful things are missing. The one that prompted this issue is **switching deck** — a user does it from the drop-down at the top of the app and cannot do it by voice at all.

Widening the set naively repeats the mistake #802 already named. #802's guard (linkage-check rule 13, `xtask/linkage-check/src/voice_command_registry.rs`) proves every **entry in `VOICE_ACTIONS`** (`desktop/src/lib/voiceActions.ts`) is classified — it carries either a command-table row (`voice: true`) or a non-empty `no_voice` reason. It does **not** prove every **capability** is registered: a control wired straight to a React state setter reaches none of the guard's assertions. #802 recorded five such second dispatch paths as its deferred milestone **D10**, and `voiceActions.ts`'s header comment lists them:

- `focusAgent` — the agent tile's own `onSelect` calls `setSelectedAgentId` directly;
- `toggleEvidenceDrawer` — the workspace header's Evidence button, and the evidence row's select-and-open;
- `openWorkflowOrder` — the run-graph "Edit loop" button (twice) and `ProjectsPanel`'s `onConfigureWorkflow`;
- `openProjects` — `WorkflowPanel`'s `onChooseProject`;
- `openAgentProfiles` — `EmptyDeck`'s `onProfiles`.

That comment argued re-routing them was regression risk for no functional gain while none was voice-reachable, and said that a later PRD giving any of the five a row owns closing its second path. **This PRD deliberately reverses that trade**: not because any of the five must become voice-reachable, but because the rule "a user-facing control dispatches through `VOICE_ACTIONS`" is only checkable once there are no sanctioned exceptions to it.

## Solution Overview

Two halves, **in this order** — the issue is explicit that the guard lands first, or the wider set is added under the same convention that let things be forgotten in the first place.

1. **Registration becomes unforgettable.** Route the five second dispatch paths through the registry, then turn "a new user-facing control must dispatch through `VOICE_ACTIONS`" into a build-checked rule. Afterwards, adding a command is one row plus one classification flip (plus the pinned-by-value test assertions #802 M8 measured), and adding a *feature* without deciding about voice is a build failure rather than an oversight.
2. **The set widens**, starting with switching deck — which needs a new resolver param kind for a deck reference, the way `agent_ref` works for agents — and then revisiting, **individually**, the overlay-opening capabilities #802 classified `no_voice`.

## Scope

### In Scope

- Re-routing the five known second dispatch paths through `VOICE_ACTIONS[id].run(...)`, with no user-visible behaviour change, and rewriting `voiceActions.ts`'s header comment to match.
- A build-checked registration rule in `xtask/linkage-check` (extending rule 13 or beside it), with planted-bad-input tests so "no findings" is meaningful.
- A `switch_deck` voice command: a registry entry, a `commands.toml` row, a new `deck_ref` param kind (Rust `ParamKind`, the guard's closed set, schema generation, prompt live state, resolution against live decks), and the deck drop-down dispatching through the same entry.
- Individual re-examination of the `no_voice` overlay capabilities (projects, prompts, agent profiles, workflow order): each either gains a row or keeps a `no_voice` reason restated against today's code.
- Phrase fixtures for every new row (credentialed lane, run locally), docs in `docs/develop/desktop-gui.md`, and a changelog fragment.

### Out of Scope

- **Destructive commands** (approve/deny, close an agent's terminal pane, stop an agent) — #802's D5, which needs a confirmation flow first.
- #1184 (command chaining), #1260 (dictation mode), #1261 (numbered choice), #1246 and #1248 (voice bugs another unit may take), even where adjacent.
- Anything #742 (fleet view) adds — it brings a daemon dimension to nearly every command and has not shipped; see D1 below.
- Any daemon or TUI↔daemon protocol change. Deck switching is frontend state; `PROTOCOL_VERSION` is untouched and no `.breaking.md` is owed unless implementation proves otherwise (rule 12 would then apply).

## Technical Approach

### Half 1 — the five paths, and why re-routing is safe now

Each second path calls a state setter the registry entry already wraps. Re-routing replaces the direct setter call at each call site with the entry's `run(...)`, passing whatever the context needs (for the evidence row's select-and-open, the entry or a context member must accept the selection too — the entry's semantics must not narrow). The existing vitest coverage of those controls must keep passing unchanged; where a control has none, a test proving it now dispatches through the registry is added before the re-route. The `no_voice` classification of each of the five is **not** changed by this half.

### Half 1 — what the guard checks, and the choice this PRD makes

The property is: **a control that changes app state a capability owns cannot reach that state except through `VOICE_ACTIONS`**, and **new capability state cannot be introduced without a classification**. #802's Open Question 3 considered the only approximation it had — pinning the count of `onClick` sites (80 when measured) — and rejected it for churn: most are chrome (dismiss a toast, close a sheet), and a count cannot tell a capability from a close button.

**Decision: guard the state, not the clicks.** Two assertions, both text scans in the idiom rule 13 already uses:

1. **Setter ownership.** The state setters that back registry entries (derived mechanically from what the registry's action context exposes, not from a second hand-written list) may be referenced in production `desktop/src/**/*.tsx?` only where the action context is constructed. Any other reference — a new `onClick={() => setProjectsOpen(true)}` — fails the build and names the file and the setter.
2. **Shell state is classified.** Every `useState` in the shell components that own navigation and overlay state (`desktop/src/App.tsx` at minimum; the implementer confirms the exact set and records it) is either owned by the registry (its setter reaches the action context) or listed in an allowlist with a one-line reason (transient UI state: a hover, a draft, a toast). A new overlay boolean fails the build until it is registered or classified.

This is cheaper than an `onClick` census because it keys on the dozen-odd pieces of state that *are* capabilities rather than the ~80 handlers that mostly are not. It is still not a proof of completeness — a capability whose state lives in a leaf component outside the scanned set is invisible — and the rule's own doc comment must say so rather than implying more. If implementation finds a materially better mechanism, it may replace this one; the deviation and its reason go in the Work Log.

### Half 2 — `switch_deck` and `deck_ref`

- A registry entry (working name `switchDeck`) that the deck drop-down at the top dispatches through — so this is also a sixth path closed, not a new second path opened.
- A `commands.toml` row `switch_deck` with one param of a new kind `deck_ref`. `screens` lists exactly the screens on which the drop-down is clickable (verify in the code; #802 M8's rule is that voice never reaches a surface a click cannot reach from that screen).
- `deck_ref` added everywhere the closed set of param kinds is written down: Rust `ParamKind` (`desktop/src-tauri/src/voice/table.rs`), the guard's closed set, schema generation, the live state the prompt shows the model (deck names/labels), and resolution against live decks mirroring `resolve_agent_ref` — including the ambiguous and unresolved outcomes, rendered by the app from the table as every other outcome is.
- Switching to the already-active deck is a no-op with an honest report, not an error.

### Half 2 — the `no_voice` overlays, one at a time

For each of projects, prompts, agent profiles and workflow order: read its current `no_voice` reason, check it against today's code, and either (a) give it a row, closing any remaining second path, or (b) keep it `no_voice` with the reason restated. #802 notes overlay booleans are not in `DeckView`, so a row whose availability depends on an overlay being open is a change to where the state lives, not a table trick. Each decision is recorded in the Work Log.

### Feature flag (CLAUDE.md rule 9)

**No new flag.** Voice control is not behind `experimental` — #802 recorded that decision and its precedent (`prds/done/176-desktop-gui.md` decision 6: the desktop binary has no flag seam, and `features::init_and_watch` is never called there). New commands on an unflagged surface follow it.

### Testing

Per #802's "what rule 4 means here": the blocking desktop tier is Rust unit tests in `desktop/src-tauri` (run by `cargo test-fast --workspace`), plus linkage-check's tests; vitest (`desktop-web`) and Playwright (`desktop-browser`) are advisory but are where dispatch-through-the-registry and the surface are proven. Real-model phrase fixtures run in the credentialed lane, locally only. No TUI L2 test applies — nothing here touches the TUI binary, daemon, or hooks.

## Success Criteria

- No production `.tsx` references a registry-owned state setter outside the action context's construction; the guard fails the build on a planted violation and on a planted unclassified shell `useState`.
- The five D10 paths and the deck drop-down dispatch through `VOICE_ACTIONS`, with no user-visible behaviour change for the five.
- "Switch to deck X" by voice switches deck through the same entry the drop-down uses; ambiguous and unknown deck names produce the table's rendered outcomes, not a silent no-op.
- An unknown param kind, a typo'd `invoke`, or an unclassified entry still fails rule 13 — `deck_ref` is in the closed set, not a hole in it.
- `cargo test-fast --workspace`, rule 2's clippy, and the required build jobs stay green; `PROTOCOL_VERSION` unchanged.

## Milestones

- [x] **M1 — Route the five second dispatch paths through `VOICE_ACTIONS`.** Tile `onSelect`, both evidence entry points, all three workflow-order entry points, `WorkflowPanel`'s `onChooseProject`, `EmptyDeck`'s `onProfiles`. No behaviour change; vitest proves each control dispatches through the registry. `voiceActions.ts`'s header comment rewritten to say the residual is closed.
- [x] **M2 — The registration guard.** Setter ownership plus shell-state classification (see Technical Approach), in `xtask/linkage-check`, with planted-bad-input tests for each assertion. Docs for "adding a control" updated in `docs/develop/desktop-gui.md`.
- [ ] **M3 — `switch_deck` and the `deck_ref` kind.** Registry entry, drop-down re-routed through it, table row, `deck_ref` across every place the closed set lives, live state, resolution and its outcomes, pinned-by-value test updates, phrase fixtures.
- [ ] **M4 — Revisit the `no_voice` overlays individually.** Projects, prompts, agent profiles, workflow order: row or restated reason, each recorded.
- [ ] **M5 — Docs, fixtures run, changelog.** `docs/develop/desktop-gui.md` updated; credentialed phrase fixtures run locally and named in the PR; `changelog.d/1195.feature.md`.

**Deferred.**

- [ ] **D1 — Fleet-view commands (#742).** Waiting on #742 shipping; it adds a daemon dimension to most commands.
- [ ] **D2 — Destructive commands.** #802's D5; waiting on a confirmation flow.

## Risks

- **Re-routing regressions.** The reason #802 left the five alone. Mitigated by testing each control before and after, and by keeping M1 a pure re-route.
- **Guard false confidence.** The state-ownership rule sees only the scanned shell; a capability living in a leaf component escapes it. The rule's doc comment must state that bound.
- **Guard churn.** An allowlist of transient state costs one line per new piece of shell state; that is the intended price of a decision being forced.

## Open Questions

1. ~~Which components beyond `App.tsx` own capability state and belong in the scanned set?~~ **Answered in M2:** `desktop/src/hooks/useShellOverlays.ts` joins `App.tsx`, because issue #1197 moved the overlay booleans there and they are the state four of the five D10 paths reached. `desktop/src/components/AgentOverview.tsx` also owns capability state — the New agent dialog (`newAgent`) and the stop confirmation (`confirm`) — but reaches it through helpers the rule does not trace, and its row Stop opens the confirmation without the registry by PRD #802 D5's decision; it is recorded as the named next candidate and as a stated bound of rule 18, not scanned. See the M2 Work Log entry.

## Work Log

### 2026-09-26 — Created

Written from issue #1195's body by the orchestrator of the dispatched unit, with milestones in the issue's stated order (guard first, then widening). The guard mechanism ("guard the state, not the clicks") is this document's choice, made because #802's Open Question 3 rejected the `onClick` census for churn; implementation may replace it with a recorded reason.

### 2026-09-26 — M1: the five second dispatch paths route through `VOICE_ACTIONS`

Every call site the old header comment listed now dispatches through the registry: the agent tile's `onSelect` (`focusAgent`), the workspace header's Evidence button and the evidence row's select-and-open (`toggleEvidenceDrawer`), the run graph's header Edit loop, its empty-state Edit loop and `ProjectsPanel`'s `onConfigureWorkflow` (`openWorkflowOrder`), `WorkflowPanel`'s `onChooseProject` (`openProjects`), and `EmptyDeck`'s `onProfiles` (`openAgentProfiles`). One more site that opened a capability through its setter was found while doing it and routed the same way: the launch flow reopening Projects when the daemon no longer knows the project (`openProjects`).

**One entry was widened rather than a call site narrowed.** The evidence row is not a flip — it shows the drawer on the item it selects whichever way the drawer was pointing — so `VoiceActionContext.toggleEvidence` takes an optional `open` and `toggleEvidenceDrawer.run` an optional `{ open }`. No target still flips, which is what the header button and the palette entry do. No entry's `voice`/`no_voice` classification changed.

Tested in `desktop/src/lib/voiceActions.test.ts`, which already wraps the real registry to record dispatches: nine new cases, one per re-routed control (the evidence row's case clicks a row with the drawer closed and again with it open, so a row that became a flip fails it). Measured: with the `App.tsx`/`voiceActions.ts` change reverted and the tests kept, the eight cases for the five listed paths fail and the existing 24 pass; with it applied the file passes 33/33, and the full `pnpm test` passes 975/975 across 35 files. `voiceActions.ts`'s header comment now says the residual is closed and why.

### 2026-09-26 — M2: linkage-check rule 18, `voice-capability-state`

**Where it lives, and a deviation from "extending rule 13".** The task and this PRD said "rule 13"; the voice registry rule is **rule 14** in `RULES` (`voice-command-registry`; rule 13 is `git-program-literal`). The guard is a **sibling module**, `xtask/linkage-check/src/voice_capability_state.rs`, registered as **rule 18**, rather than more of rule 14: it reads different files (the app shell, not the table), needs a different lexer (TSX with JSX, regex and template literals — rule 14's scanner handles plain TypeScript and documents regex literals as a gap), and its failure sentence has to name a different residual.

**What it asserts, as built.** The PRD's "guard the state, not the clicks", with the derivation made concrete:

- *Context types* are derived, not listed: every type alias in production `desktop/src/**/*.ts(x)` whose right-hand side names `VoiceActionContext`, plus that type. Measured on the tree: `VoiceActionContext`, `VoiceScreenContext`, `VoiceDispatchContext`, `VoicePanelContext`, `VoiceShellContext`, `VoiceOverviewContext`, `NewAgentVoice`, `RailContext` (in `NavigationRail.tsx`), and two type-level helpers that annotate no literal and so own nothing (`VoiceActionEntry`, `NeedsCoversRun`).
- *Construction sites* are object literals annotated with one of those (`const x: T = {`, `useMemo<T>(() => ({`, optionally via `Partial<…>`). `App.tsx` has four: `DeckShell`'s rail context and dispatch context, `ControlDeck`'s rail context, and `DeckSurface`'s screen context.
- *Registry-owned setters* are the `set*` identifiers named inside those literals. Measured: `setView`, `setOverlay`, `setSelectedAgentId`, `setEvidenceOpen`, `setTabs`, `setTerminalFocus`.
- **Assertion 1 (setter ownership):** in the scanned files, an owned setter referenced anywhere else fails, naming file, line and setter — unless the reference is its declaration, a hook dependency array, a **dismissal** (a call whose last argument is the literal `false`), or marked with a `voice-registry-exempt: <reason>` comment.
- **Assertion 2 (shell state classified):** every `useState`/`useReducer` in the scanned files is registry-owned (its setter is owned) or carries that comment. A `useState` the rule cannot read (not destructured on one line) is a finding, not a skip.
- A marker with no reason, and a marker that exempts nothing, are findings too.

**Three decisions the PRD left open, and why.** (1) *The allowlist is inline comments, not a list in the rule.* The reason sits beside the state it classifies, the way a `no_voice` reason sits beside its entry, and a stale one is a finding, so it cannot outlive its code. (2) *Dismissals are exempt by shape.* Closing a panel changes capability state but is not a capability; `DeckSurface`'s `Escape` handler already argued that a blanket dismissal is not a registry dispatch, and without this every close X would need a comment and the reasons would stop meaning anything. What the rule guards is the opening/selecting half. (3) *A trailing marker covers only its own line.* The first version let a marker also cover the line below, and the real-tree mutation below showed it silently classifying a new overlay boolean added under the palette's `useState`; a marker now covers the next line only when it stands alone on its line.

**What `App.tsx` had to change for the rule to hold, none of it visible.** `DeckSurface`'s five `set<Overlay>Open` wrappers and its `overlaySetters` record are gone: the context now names the setter directly (`openOverlay: (overlay) => setOverlay(overlay, true)`), closes are `setOverlay("<overlay>", false)`, and the blanket close iterates a module-level `DECK_OVERLAYS`, written as a `Record<DeckOverlay, true>`'s keys so a new overlay is still a type error there. `focusTerminal`'s body moved inside the context literal, because a helper beside the literal hides its setters from the derivation. The remaining non-control writes carry written reasons: the view's own back (`closeAgent`), the voice report's Undo, `setView` handed to each screen as its navigator, the rail's shortcut-sheet button and `setHelpOpen` (a `ShellOverlay` no entry opens), the deck keeping its selection on a live agent, and a tile's own tab strip. The eight transient `useState`s in `App.tsx` and the overlay hook's one are classified the same way. **The PRD's "no production `.tsx` references a registry-owned setter outside the construction site" is therefore not literally true, and was never going to be**: a dismissal, a dependency array and those written exceptions remain. The honest version is the one the rule checks.

**Bound, stated in the module comment and in every failure.** State outside `App.tsx` and `useShellOverlays.ts` is invisible (`AgentOverview.tsx` is the known case — see Open Question 1), and so is a capability reached through anything that is not a `set*` identifier: a callback prop such as `onNavigate`, or a helper named in a context.

**Measured.** Rule 18's tests are in its `#[cfg(test)]` module, with the neighbours' budget (reads files, no network/git/subprocess/sleep; an unreadable root, a missing shell file, a missing `VoiceActionContext`, a file the lexer loses its place in, and an app with no construction site are each a finding): 20 tests, one proving the real tree passes, one proving the scan is not vacuous (real context types derived, at least three sites found, the M1 setters owned), and planted-bad-input cases for each assertion — a direct setter call in a control, a setter handed away as a value, a setter newly added to a context becoming owned with no list edited, an unclassified `useState` (including one directly under a classified one), an unreadable `useState`, the overlay hook, markers with no reason and markers that exempt nothing, only a literal-`false` last argument counting as a dismissal, and the lexer ignoring comments, strings and regex literals while seeing into a template's `${…}`. **Real-tree mutation:** changing the run graph's Edit loop back to `onClick={() => setOverlay("workflow", true)}` made `cargo xtask linkage-check` report `[18] desktop/src/App.tsx:…: \`setOverlay\` is written outside the action context it is named in`, and adding `const [reportsOpen, setReportsOpen] = useState(false);` under the palette's state reported `the state \`reportsOpen\` … is not registry-owned and not classified` (after the trailing-marker fix above; before it, that mutation passed). Restored, `cargo xtask linkage-check` reports `ok (… 18 rules)`. `cargo nextest run --workspace -p xtask-linkage-check`: 577/577. `pnpm test` in `desktop/`: 975/975, `tsc --noEmit` clean.

**A red met on the way, fixed here per CLAUDE.md rule 6.** `cargo test-fast` failed four `remote_tunnel::tests::the_probe_*` tests on this host, deterministically and in isolation, with this branch touching nothing in `src/`. Cause: `run_socket_probe` removed only the three fallback variables, and issue #1174's resolver rung in `REMOTE_SOCKET_PROBE` runs whatever `dot-agent-deck` `HOME`/`PATH` lead to — so on a machine with `~/.local/bin/dot-agent-deck` installed, that binary answered (with an empty line) before the fallback under test. CI has no deck installed, which is why it stayed green. The sibling helper `tunnel_tests::run_probe_under` already documented and avoided exactly this with `env_clear`; `run_socket_probe` now does the same (test-only change). Afterwards all 77 `remote_tunnel` tests pass, and `cargo test-fast` passes 4518/4518.

**Docs.** `docs/develop/desktop-gui.md` gained "Adding a control: rule 18 and the state it guards", and the command checklist and the registry section point at it. PRD #802's D10 is ticked with a pointer here.
