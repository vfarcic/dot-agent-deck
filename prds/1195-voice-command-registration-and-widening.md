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
2. **The set widens**, starting with switching deck — which reuses the `deck_ref` resolver kind PRD #1223 added for the New agent flow (this document first said a new kind was needed; M3 found it already existed) — and then revisiting, **individually**, the overlay-opening capabilities #802 classified `no_voice`.

## Scope

### In Scope

- Re-routing the five known second dispatch paths through `VOICE_ACTIONS[id].run(...)`, with no user-visible behaviour change, and rewriting `voiceActions.ts`'s header comment to match.
- A build-checked registration rule in `xtask/linkage-check` (extending rule 13 or beside it), with planted-bad-input tests so "no findings" is meaningful.
- A `switch_deck` voice command: a registry entry, a `commands.toml` row with a `deck_ref` param — the kind PRD #1223 already added (Rust `ParamKind`, the guard's closed set, schema generation, prompt live state, resolution against live decks), resolved here against the decks the selector lists — and the deck drop-down dispatching through the same entry.
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
- A `commands.toml` row `switch_deck` with one param of kind `deck_ref`. `screens` lists exactly the screens on which the drop-down is clickable (verify in the code; #802 M8's rule is that voice never reaches a surface a click cannot reach from that screen).
- **`deck_ref` is not new — PRD #1223 added it** for the New agent flow (`open_new_agent`, `choose_deck`), and it already sits everywhere the closed set of param kinds is written down: Rust `ParamKind` (`desktop/src-tauri/src/voice/table.rs`), rule 14's closed set, schema generation, the live state the prompt shows the model (deck labels), and `resolve_deck_ref`, mirroring `resolve_agent_ref` with the ambiguous and unresolved outcomes. This document originally listed adding it as M3 work; the correction is recorded in the M3 Work Log entry, together with what M3 did have to add: the set it resolves against.
- Switching to the already-active deck is a no-op with an honest report, not an error.

### Half 2 — the `no_voice` overlays, one at a time

For each of projects, prompts, agent profiles and workflow order: read its current `no_voice` reason, check it against today's code, and either (a) give it a row, closing any remaining second path, or (b) keep it `no_voice` with the reason restated. #802 notes overlay booleans are not in `DeckView`, so a row whose availability depends on an overlay being open is a change to where the state lives, not a table trick. Each decision is recorded in the Work Log.

### Feature flag (CLAUDE.md rule 9)

**No new flag.** Voice control is not behind `experimental` — #802 recorded that decision and its precedent (`prds/done/176-desktop-gui.md` decision 6: the desktop binary has no flag seam, and `features::init_and_watch` is never called there). New commands on an unflagged surface follow it.

### Testing

Per #802's "what rule 4 means here": the blocking desktop tier is Rust unit tests in `desktop/src-tauri` (run by `cargo test-fast --workspace`), plus linkage-check's tests; vitest (`desktop-web`) and Playwright (`desktop-browser`) are advisory but are where dispatch-through-the-registry and the surface are proven. Real-model phrase fixtures run in the credentialed lane, locally only. No TUI L2 test applies — nothing here touches the TUI binary, daemon, or hooks.

## Success Criteria

- In the scanned shell files (`desktop/src/App.tsx`, `desktop/src/hooks/useShellOverlays.ts`), a registry-owned state setter is written only inside an action context construction site, or as a dismissal, a hook dependency array, or on a line carrying a written `voice-registry-exempt:` reason, and every `useState` there is registry-owned or carries one (rule 18); the guard fails the build on a planted violation and on a planted unclassified shell `useState`. State held outside those files and a capability reached through a callback rather than a `set*` setter are outside it — the M2 Work Log records why the unqualified "no production `.tsx` references a setter outside the construction site" was never going to hold. (Narrowed 2026-09-26 from that unqualified wording, per CLAUDE.md rule 17.)
- The five D10 paths and the deck drop-down dispatch through `VOICE_ACTIONS`, with no user-visible behaviour change for the five.
- "Switch to deck X" by voice switches deck through the same entry the drop-down uses; ambiguous and unknown deck names produce the table's rendered outcomes, not a silent no-op.
- An unknown param kind, a typo'd `invoke`, or an unclassified entry still fails rule 13 — `deck_ref` is in the closed set, not a hole in it.
- `cargo test-fast --workspace`, rule 2's clippy, and the required build jobs stay green; `PROTOCOL_VERSION` unchanged.

## Milestones

- [x] **M1 — Route the five second dispatch paths through `VOICE_ACTIONS`.** Tile `onSelect`, both evidence entry points, all three workflow-order entry points, `WorkflowPanel`'s `onChooseProject`, `EmptyDeck`'s `onProfiles`. No behaviour change; vitest proves each control dispatches through the registry. `voiceActions.ts`'s header comment rewritten to say the residual is closed.
- [x] **M2 — The registration guard.** Setter ownership plus shell-state classification (see Technical Approach), in `xtask/linkage-check`, with planted-bad-input tests for each assertion. Docs for "adding a control" updated in `docs/develop/desktop-gui.md`.
- [x] **M3 — `switch_deck` on the existing `deck_ref` kind.** Registry entry, drop-down re-routed through it, table row, the selector's decks in the resolution set, pinned-by-value test updates, phrase fixtures. (Written as "`switch_deck` and the `deck_ref` kind" — `deck_ref` came from PRD #1223; see the M3 Work Log entry.)
- [x] **M4 — Revisit the `no_voice` overlays individually.** Projects, prompts, agent profiles, workflow order: row or restated reason, each recorded. (All four keep `no_voice` with restated reasons; two drafted rows are deferred as D3 — see the M4 Work Log entry.)
- [ ] **M5 — Docs, fixtures run, changelog.** `docs/develop/desktop-gui.md` updated; credentialed phrase fixtures run locally and named in the PR; `changelog.d/1195.feature.md`. Docs and changelog are done (M5 Work Log entry); **the credentialed fixture run is pending** and will be recorded when it lands, which is what this box is left unticked for.

**Deferred.**

- [ ] **D1 — Fleet-view commands (#742).** Waiting on #742 shipping; it adds a daemon dimension to most commands.
- [ ] **D2 — Destructive commands.** #802's D5; waiting on a confirmation flow.
- [ ] **D3 — Rows for `open_agent_profiles` and `open_workflow_order`.** Drafted in the M4 Work Log entry. Waiting on those panels graduating from the experimental flag (issue #1198), or on generalising `schema::hidden_by_flag` and `DeckShell`'s dispatch gate from `open_deck` to a per-row feature, whichever comes first.

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

### 2026-09-26 — M3: `switch_deck`, on the `deck_ref` kind PRD #1223 had already added

**A correction first.** This document said M3 needed a new resolver kind for a deck reference. It did not: PRD #1223 added `deck_ref` for the New agent flow (`ParamKind::DeckRef`, parsing, schema, the prompt's deck labels, rule 14's closed set, `resolve_deck_ref`, and the ambiguous/unresolved outcomes), and `switch_deck` reuses it with no new variant. The Technical Approach, the scope and M3's own line are corrected above.

**What M3 did have to add: the set it resolves against.** Voice's decks were the snapshot's `observed` fleet — what the app connects to — and under a single-deck selection that is exactly the deck already on screen (`EndpointSettings::connectable_endpoints`). Resolved against that, "switch deck to the build box" could only ever answer "no deck matches" for every deck but the current one, and no unit test would have noticed, because every test plants its own fleet. `selector_voice_decks` (`desktop/src-tauri/src/lib.rs`) appends every deck the Deck selector lists — `local` and each `[[endpoints.remote]]` row, from the same settings document the selector renders — keyed as the fleet would key it (wire id, or `unconfigured-<id>` for a row with no socket) and marked unable to take a new agent (`voice::DECK_NOT_CONNECTED`, or the fleet view's "not configured" sentence), so the New agent flow neither offers nor preselects one. **All Decks is not voice-reachable**: it is a selection, not a deck.

**Two things the row does that no column carries, keyed on a named row the way `SUBMIT_ROW` already is.** `outcome::SWITCH_DECK_ROW` (1) ignores a deck's New agent eligibility — switching to a deck is how it becomes one a new agent can start on, and "Deck X cannot take a new agent" is the wrong answer to "switch to X" — and (2) gets its dispatched value replaced by the selector's stored token (`voice::address_deck_switch`, from a map `lib.rs` builds), because the webview keys the selector by `local`/row id and has no map from a fleet key to either for a deck it is not connected to. A deck with no token dispatches an empty value, which `switchDeck` refuses, rather than its key. A row-level column was the alternative for both and was not taken: `CommandRow` and `ParamSpec` are built by literal in existing tests, and the tester's M3 tests pin `ParamSpec` by full equality.

**The row.** `screens = ["deck", "overview"]` (the selector is inert behind the agent pane) and **`requires = ["new_agent_dialog_closed"]`**, a decision this PRD did not anticipate: the New agent dialog is modal over the top bar, so no click reaches the selector while it is open, and a named deck then means the dialog's Deck field (`choose_deck`). That makes the tester's `choose-new-agent-deck-not-switch` fixture a precondition rather than a tie the model breaks. The description follows #802's 2026-09-19 lesson — it lists its utterances instead of generalising, states what it is NOT (`open_deck`, `open_overview`, `choose_deck` while the dialog is open), and says the deck may be one `decks` does not list, because the model is shown only the decks a new agent can start on (a disabled deck is withheld from every row; `TOOL_INSTRUCTIONS` got one sentence saying `switch_deck` is the exception). Report `Showing {deck}.`, which stays true for the deck already shown: that is a no-op that writes nothing, not an error.

**The frontend.** `VOICE_ACTIONS.switchDeck` (`voice: true`) calls `VoiceActionContext.switchDeck`, served by the shell (`VoiceShellContext`, beside `closeSettings`) over `chooseDeckSelection` in `DeckSelector.tsx` — the menu's own write, moved into a function the menu and the shell share. The menu now dispatches through the entry, so this closes a sixth path rather than opening a second one. Rule 18 needed no exemption: the write is `settings.save`, not a `set*` setter, and the new context member names none.

**Tests.** The tester's RED set went green with production changes only, except two pinned-by-value assertions the row's placement forces and the tester did not update — `voice_prompt_param_names_are_the_union_in_table_order` and `voice_openai_request_makes_every_param_nullable` pin the param union's first-appearance order, and `switch_deck` above the dictation pair moves `deck` ahead of `prefix` (the M8 cost of a row). Added: `voice_outcome_switch_deck_reaches_a_deck_the_new_agent_dialog_disables`, `voice_outcome_address_deck_switch_substitutes_the_selector_token`, `selector_voice_decks_add_the_decks_the_selector_lists` (end to end through the pipeline to the token), and three vitest cases in `DeckSelector.test.tsx` for the voice-side switch, no-op and stale-token refusal. The phrase-fixture harness had two per-fixture checks written for the New agent deck that would fail every `switch_deck` fixture — and its "Preselected deck:" check already failed every `choose_deck` fixture, whose report is "Deck: …" — so both are now scoped (`tests/voice_phrase_fixtures.rs`).

**Not run: the credentialed phrase fixtures** — no `OPENAI_API_KEY` on this host. That is M5's, and one existing fixture is at risk from this row: `choose-deck-unavailable` ("use the build box deck" on the deck screen expects `choose_deck`/`unavailable`), where `switch_deck` is now callable and arguably the right answer.

### 2026-09-26 — the M1/M2 review findings

Eight findings from the review and audit of M1 and M2, each fixed rather than deferred. **Rule 18's lexer and scanner** (`xtask/linkage-check/src/voice_capability_state.rs`): an unpaired quote in JSX text ("Don't") no longer opens a string that blanks a same-line setter write — a quote that reaches its line's end opened no string, since a TS string cannot hold a raw newline, and the rest of the line is read as code (two unpaired quotes on one line still pair, which the module comment and the rule sentence now list as a residual); the block-bodied `useMemo<T>(() => { …; return { … }; })` is recognised as a construction site, taking only a `return` at the callback body's top level; the dependency-array exemption now holds only for the last argument of `useEffect`/`useLayoutEffect`/`useInsertionEffect`/`useMemo`/`useCallback`/`useImperativeHandle`, so `register(handler, [setOverlay])` is a finding; and a symlinked directory under `desktop/src` is reported as an unsupported layout instead of silently shrinking the scan. Each has a planted test, and each test was seen to fail with its fix disabled. `main.rs`'s rule 14 doc comment had ended up above rule 18's function; each now has its own. **The socket-probe tests** (`src/remote_tunnel.rs`) run with a private `PATH` holding only a link to `id`, and `HOME` inside it, in both `run_socket_probe` and `tunnel_tests::run_probe_under` — `/usr/bin:/bin` would still have let a system-wide `dot-agent-deck` answer before the rungs under test. **This document's first success criterion** was narrowed to what rule 18 checks (rule 17), and `docs/develop/desktop-gui.md` now spells `VoiceScreenContext` as the type actually is.

### 2026-09-26 — M4: the four `no_voice` overlays, each kept, each reason restated

All four keep `no_voice`. Each reason was read against today's code and rewritten in `desktop/src/lib/voiceActions.ts`; two of the old four no longer held, and the decisions differ in kind, so they are recorded one at a time.

**The prerequisite common to all four: voice has no per-panel flag gate.** All four panels are hidden unless the experimental flag is on (issue #1198). Voice's only flag gate is `open_deck`'s: `schema::hidden_by_flag` marks a row uncallable only when its `invoke` is `openDeck`, and `DeckShell`'s dispatch refuses only that invoke while `showDeck` is off. The four panels' flag fields (`showProjects`, `showPrompts`, `showAgentProfiles`, `showWorkflows`) are separate fields that follow one flag today, and `App.tsx`'s palette comment already names what happens if they diverge. So a row for any of these would, today, be a spoken door to a panel the flag is meant to hide, with nothing on the voice side to close it.

- **`openProjects` — kept.** Beyond the flag, it opens a picker over project paths the daemon supplies, and no resolver kind in the table can name one of them: a row would open the panel and leave the user in a list voice cannot choose from, a worse dead end than no command. It becomes a candidate once a `project_ref` kind exists. The reason is unchanged in substance; its wording now says what would change the answer.
- **`openPromptLibrary` — kept.** Beyond the flag, it is a browse-and-edit surface, and none of its operations — choosing, adding, editing or removing a stored prompt — has a row, so the command would open a panel and stop. The old wording blamed "this PRD's navigation-only slice"; the restated one names what is actually missing (the rows).
- **`openAgentProfiles` — kept, and the old reason was wrong.** It said the profiles form is "the configuration surface PRD #802 D5 puts behind confirmation". D5 (`prds/done/802-desktop-voice-control.md`) is about approving or denying a permission prompt, closing a pane and stopping an agent; it does not name configuration, and opening this form changes nothing until the user edits and saves a profile. What does hold is the flag-gate prerequisite above, and that is now the whole reason. It gets a row at graduation, or once the gate is per-row — deferred D3.
- **`openWorkflowOrder` — kept, and the old reason was wrong in the same way.** It said "nothing in this slice starts anything", which PRD #1223's `start_new_agent` and #802's D5 decision ("a spoken start starts") had already made false. Opening the editor launches nothing — only its own Launch does — so, like agent profiles, the flag gate is the whole reason, and it gets a row on the same terms (D3). Launching an orchestration by voice would be a **separate** row, weighed against D5 on its own merits, and is not part of D3.

**The two rows D3 would add, as drafted** — both `screens = ["deck", "overview"]`, because the rail's `openOverlay` opens a deck panel from the overview by going to the deck first (`DeckShell`'s `railContext`), and not the agent screen, whose rail sits inert behind the modal pane, the same reason `open_settings` gives:

| id | invoke | description (gist) | report |
| --- | --- | --- | --- |
| `open_agent_profiles` | `openAgentProfiles` | Open the Agent profiles sheet, where each role's model, permissions and launch command are set. Use for "agent profiles", "configure agents", "which model does the coder use". This only opens the sheet — it changes no profile by itself. | `Opening agent profiles.` |
| `open_workflow_order` | `openWorkflowOrder` | Open the workflow editor, where roles are enabled, skipped and reordered. Use for "edit the workflow", "workflow order", "reorder the roles". This only opens the editor — it launches nothing; the editor's own Launch does. | `Opening the workflow editor.` |

Either row lands with its `voice: true` flip, its M8 cost (the "what adding a row costs" table in `docs/develop/desktop-gui.md`) and phrase fixtures, and only after one of D3's two conditions holds: the panel graduating from the flag (its `graduate-<feature>` issue), or `schema::hidden_by_flag` and `DeckShell`'s dispatch gate generalised from `open_deck` to a per-row feature — which is the better route if a third flagged surface wants a row first.

**Tests.** No test pins these four strings; `voiceActions.test.ts`'s classification check asserts each entry carries `voice: true` or a non-empty `no_voice`, and still passes. Rule 14 sees the same classification, so `cargo xtask linkage-check` is unaffected by the text.

### 2026-09-26 — M5: docs and changelog (fixture run pending)

`docs/develop/desktop-gui.md` already carried rule 18 (M2's "Adding a control" section) and `switch_deck` (M3, in the New agent flow section); M5 adds the M4 decisions to the registry section, next to the classification rule they apply. `changelog.d/1195.feature.md` describes "switch to deck X" as a user sees it and leaves rule 18 out, per CLAUDE.md rule 19 — nobody using the app can observe a build guard. No `.breaking.md`: nothing here moves the TUI↔daemon contract and `PROTOCOL_VERSION` is untouched. **The credentialed phrase-fixture run is still pending**; the tester is running it and it is recorded here when it completes, which is why M5 stays unticked.

### 2026-09-26 — the final review findings: a switched-to deck must be named, and unchanged

Five findings from the final review and audit, fixed against the tester's RED tests with production changes only.

**The blocker: `switch_deck`'s deck is now held against the transcript.** It was resolved against the configured decks with nothing checking the user had named it, so "switch deck to local" answered with `deck="build box"` switched to the build-box remote — an SSH connection the user did not ask for, with no dialog in the way. `resolve_param` (`desktop/src-tauri/src/voice/outcome.rs`) now refuses a `SWITCH_DECK_ROW` deck whose `spoken` value fails `said` (every content word heard in the transcript) as a new `Unmet::NotSaid`, rendered "I did not catch which deck" and checked before resolution, so an invented name is never quoted back as "no deck matches …". The words that pass then resolve through `deck_spoken_names` as before, so the local deck answers to "local" and "this machine" and "the build box" still reaches `deploy@build-box.example.com`. It reuses `said` and `Heard`, the matcher behind action grounding; no second matcher. This reinstates reference grounding for one row only, and the reason it does not reopen the 2026-09-24 removal is that it checks the model's own copy of the user's words, not a title spoken word for word. **`choose_deck` and `open_new_agent` are deliberately not grounded the same way:** they preselect a deck in the New agent dialog, which starts nothing until the user presses Start and is changed by one more utterance — the undo-by-one-utterance case `resolve_param`'s doc comment gives for leaving references ungrounded — whereas a switch connects at once.

**A rebound deck is refused.** A row id survives Settings editing that row's host, SSH user, port or socket, so a switch resolved against one machine could write a selection that reaches another. `selector_voice_decks` (`lib.rs`) now maps each deck to a `voice::VoiceDeckSelection` — the token plus, for a remote row, a `VoiceDeckIdentity` of those four fields read from the same row — and `address_deck_switch` puts the identity on the param as `ResolvedParam::deck_identity` (serialized `deckIdentity`, absent otherwise). The Tauri bridge restores the two keys Rust omits (`user`, `socket`) so `VoiceDeckIdentityDto` is true of what a caller holds; `App.tsx` carries it as `VoiceDispatchTarget.deckIdentity`; `VOICE_ACTIONS.switchDeck.run` passes it as `switchDeck`'s second argument; and `chooseDeckSelection` compares it with the current row before writing, refusing a mismatch with "That deck changed in Settings since you asked for it — try again." **The local deck carries no identity** — it has no remote address that can change under its token — and neither does the menu, which writes the row it rendered; an identity arriving with a token that names no remote row is refused.

**Rule 18's receiver.** `callee` returned only the trailing identifier, so `registry.useEffect(handler, [setX])` passed as a dependency array. A member call now counts only through `React.`; any other receiver is not a hook.

**Two nits:** `App.tsx`'s nested ternary for the deck target is parenthesised and split, and `resolve_param`'s `find(|deck| for_new_agent && …)` is an explicit `if for_new_agent { … } else { None }`.

**The fixtures, reworded and added by the tester, all UNRUN against a live model** — no `OPENAI_API_KEY` on this host, so the credentialed half of `tests/voice_phrase_fixtures.rs` skipped. `choose-deck-unavailable` is now "set the new agent's deck to the build box", since "use the build box deck" with the dialog closed is a Deck selector request; that utterance is the new `switch-deck-use-build-box` (dispatch to `deck-build-box`); and `switch-deck-local` adds the local switch.

**Tests.** Green from red: `voice_outcome_switch_deck_resolves_or_reports_the_deck_reference`, `refuses a deck whose endpoint identity changed under the same row id`, `a_setter_list_passed_to_a_non_hook_fails`. Extended: `voice_outcome_address_deck_switch_substitutes_the_selector_token` and `selector_voice_decks_add_the_decks_the_selector_lists` (the identity rides along, local has none, a row with no socket still has one); added `gives a switch's deck identity all four keys` in `bridge.test.ts`. `docs/develop/desktop-gui.md`'s `switch_deck` paragraph gained both guards and rule 18's sentence the receiver restriction; `changelog.d/1195.feature.md` says a deck the user did not name, or whose address changed, is not switched to.
