# PRD #1223: Create an agent from the desktop overview — the TUI's Ctrl+n, in one voice-operable dialog

> **How to read this document.** Everything above the Work Log is the plan as written on 2026-09-22, kept as the historical record. Implementation changed several of its decisions after the user tried the shipped flow: the three steps became **one dialog**, the typed-path field was **removed**, the flow gained **closing** and **voice control**, and voice is no longer `no_voice`. Where a section below is superseded it says so inline; the Work Log is authoritative.

**Status**: Implemented — PR open, awaiting review and merge
**Priority**: Medium
**Created**: 2026-09-22
**Issue**: [#1223](https://github.com/vfarcic/dot-agent-deck/issues/1223)

## Problem Statement

The desktop app cannot start an agent from its supervisory surface. The one creation path it has is the Runs screen's workflow launch, and that path needs a resolved project — a directory holding a `.dot-agent-deck.toml`. Point it at an ordinary repository and the daemon refuses with `unresolved: that path did not resolve to a project on this daemon` ([#1041](https://github.com/vfarcic/dot-agent-deck/issues/1041)). The TUI's most ordinary use — open a directory and start one agent in it — has no desktop equivalent.

**The plumbing half exists and nothing reaches it.** `DesktopAction::StartAgent { command, cwd, display_name, rows, cols }` is defined in `desktop/src-tauri/src/dto.rs` and handled by `desktop_run_action` in `desktop/src-tauri/src/lib.rs`, but at `ffcf48d8` no frontend code dispatches it: the frontend's `DeckAction` union has no `start_agent` member, `TauriDeckBridge.runAction` forwards a fixed list that excludes it, and the `agent_id` the backend returns is dropped because `DeckActionResult` has no field for it.

**Even if it were reachable, it would start the agent on the wrong deck.** The `StartAgent` arm reaches its daemon through `trusted_daemon()`, which reads the globally applied selection, and `EndpointSettings::resolve()` maps `Selection::All` to the local deck ([#1083](https://github.com/vfarcic/dot-agent-deck/issues/1083)). The overview is the screen that shows every deck at once, so it is precisely the screen where "the selected deck" is least likely to be the one the user meant.

**And the first step of the TUI's flow has no translation yet.** Ctrl+n opens a directory picker that reads the TUI process's own filesystem (`DirPickerState` in `src/ui.rs`, `std::fs::read_dir`). That is correct for the TUI, which always runs on the daemon's host — locally, or on the remote machine under `dot-agent-deck connect`'s `ssh -t`. The desktop may be on a different machine from the deck it is driving, and the daemon exposes no verb that lists directories: [PRD #819](https://github.com/vfarcic/dot-agent-deck/issues/819) excluded one deliberately and said a browse verb could arrive later only as "an explicit, separately-argued verb with its own bounds" (`prds/done/819-move-project-resolution-daemon-side.md`). [#1048](https://github.com/vfarcic/dot-agent-deck/issues/1048)'s Part 2 asks for that verb. **This PRD is that argument.**

## Solution Overview

A **New agent** flow opened from the overview that does what the TUI's Ctrl+n does, with one step added in front of it because the desktop drives several decks:

1. **Deck** — choose which connected deck the agent will run on.
2. **Directory** — browse that deck's filesystem, the way the TUI's picker browses its own.
3. **Form** — the TUI's New Agent form: a **Mode** row (`No mode`, one chip per orchestration the directory defines, `schedule`, `schedule: issues` where the deck's experimental flag is on, and `dispatcher`), an **Agent** picker over the agent registry, a **Name**, and a **Command**.
4. **Start** — the agent (or the orchestration's roles) starts on the chosen deck, and the desktop opens the new agent's pane — the desktop's equivalent of the TUI focusing the new pane.

Four commitments shape it.

**It mirrors Ctrl+n, not the Runs screen.** The Runs screen's workflow form diverges from the TUI in ways this PRD does not inherit — a mandatory task prompt and no run name ([#1044](https://github.com/vfarcic/dot-agent-deck/issues/1044)). Orchestrations launched from this flow behave like the TUI's: no task prompt, and the Name field is the run's title. The two flows share **daemon verbs** (`PrepareWorkflow` / `StartPreparedAgent`), not UI or form rules. The Runs screen is revisited separately.

**Workspace modes are excluded.** They are scheduled for removal ([#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199)), and the desktop has no mode-tab surface to render their side panes. Every other Mode chip the TUI offers is in scope.

**The daemon serves every fact about the deck.** The directory listing, which orchestrations a directory defines, the deck's default command, the agent registry, the experimental flag state, and the text of the authoring seed prompts all come from the daemon, per CLAUDE.md rule 18 and [#1043](https://github.com/vfarcic/dot-agent-deck/issues/1043). The desktop derives no path from its own environment, which is PRD #819's rule and linkage-check 12's tripwire.

**Every action names its deck.** The flow captures the chosen deck's wire id once and resolves it with `DeckScope::resolve(deck_id)` — the mechanism PRD #1105 added for cross-deck terminal attach — rather than reading the global selection. With **All Decks** selected, the agent lands on the deck the user picked, never silently on local.

## Scope

### In Scope

- A **New agent** entry point on the overview: a top-bar action, a keyboard shortcut, an affordance on each deck group's header (which preselects that deck), and the first-run empty state, whose copy currently tells the user to "start an agent from the CLI" (`desktop/src/components/AgentOverview.tsx:1184`).
- The **deck step**: every deck in the overview's fleet is listed; those that can take a spawn (connected, not pending, configured, compatible or explicitly accepted via "Connect anyway") are selectable, and the rest are shown disabled with the reason. When exactly one deck is eligible, or the flow was opened from a deck's header, that deck is preselected and the step is confirmed with one keystroke.
- A **daemon-side directory-listing verb**, capability-gated, bounded, with its threat model recorded (see [The directory-listing verb](#the-directory-listing-verb)). It closes #1048's Part 2.
- The **directory step**, keyboard-first like the TUI's picker (move, enter, go up, confirm the current directory, filter), plus a typed-path field, which is also what an older deck without the listing verb falls back to. *(Superseded: the typed-path field was removed, and a deck that cannot browse is now ineligible at the deck field with that reason — see the Work Log.)*
- The **form**, at parity with `NewPaneFormState` minus workspace modes: Mode chips, Agent picker (selecting an agent overwrites Command with that agent's default command), Name (prefilled with the directory's basename; for an orchestration, the next free `<basename>-orchestrator-N` until the user edits it; refused when it matches a live orchestration's title on that deck), Command (prefilled — see [Prefilling Command](#prefilling-command); hidden when an orchestration is selected). *(Superseded: the Agent picker was removed from both clients — see the Work Log.)*
- **Daemon support** for what the form needs: a capability-gated query for the deck's new-agent options, and daemon-side composition and delivery of the `schedule`, `schedule: issues` and `dispatcher` seed prompts.
- **Deck-targeted start actions** in the desktop backend — plain agent, authoring agent, orchestration — each resolving the chosen deck through `DeckScope::resolve` and returning the new agent's id (for an orchestration, the start role's).
- **After creation**: the desktop waits until the target deck's fleet entry lists the new agent, then opens its pane in the PRD #1105 overlay.
- A **voice registry** entry for the new action, as linkage-check rule 14 requires (`xtask/linkage-check/src/voice_command_registry.rs`) — either `voice: true` or a `no_voice` reason.
- Tests at every tier the desktop and daemon have, documentation, and CLAUDE.md rule 12's cross-version check.

### Out of Scope

- **Workspace modes** ([#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199)).
- **The Runs screen and its workflow form** ([#1044](https://github.com/vfarcic/dot-agent-deck/issues/1044), [#1083](https://github.com/vfarcic/dot-agent-deck/issues/1083)'s Runs half). Revisited separately. This PRD fixes the silent-local fallback only for its own flow, by naming the deck.
- **Moving the TUI's picker onto the listing verb.** The TUI keeps reading its own filesystem, since it always runs on the daemon's host. The three authoring seed constants move out of `src/ui.rs` so the TUI and the daemon compose from one source. *(Corrected 2026-09-23. This bullet used to read "Changing the TUI's picker or form", which was wrong as written and was read that way during implementation: it turned a narrow decision about the listing verb into a blanket "do not touch the TUI". **The standing principle is the opposite — the TUI and the desktop should have parity wherever parity makes sense.** A deck-level fact both clients can use belongs to both: `default_dir` is stored in the deck's own `DashboardConfig`, so the TUI's picker honours it too rather than it being a desktop-only setting. Where the two genuinely differ — the desktop may be on another machine, the TUI never is — the difference is the reason, not the client.)*
- **Remembering a directory per deck** — #1048's Part 1. The TUI does not remember one either (its picker always opens at the TUI process's current directory), so parity does not need it.
- **A native folder picker.** It would show the client's filesystem, which for a remote deck is the wrong machine — PRD #741's reasoning, unchanged.
- **Authentication on the attach protocol** ([PRD #741](https://github.com/vfarcic/dot-agent-deck/issues/741)). The listing verb's bounds are not a substitute for it; see the threat model.
- **An experimental-flag mechanism for the desktop** ([#1198](https://github.com/vfarcic/dot-agent-deck/issues/1198)). This flow ships visible.
- **Making orchestration-title uniqueness authoritative in the daemon** ([#555](https://github.com/vfarcic/dot-agent-deck/issues/555)). This flow mirrors the TUI's client-side refusal; #555 is where it becomes a daemon guarantee.
- **The desktop's Scheduled Tasks Add/Edit reusing the directory step.** The TUI reuses its picker there (`DirPickerIntent::ScheduleAdd` / `ScheduleEdit`); a desktop equivalent can reuse this step later.

## Technical Approach

### What Ctrl+n does today — the reference

Captured at `ffcf48d8`, from `src/ui.rs` unless stated:

- **Binding.** `KbAction::NewPane`, default `Ctrl+n` (`src/keybindings.rs`), live in every mode except the close confirmation; there is no separate orchestration dialog — orchestrations are Mode chips in the same form.
- **Directory picker** (`DirPickerState`). Opens at the TUI process's current directory. Lists `..` plus subdirectories, sorted; hidden directories are skipped; entries are tested with `DirEntry::file_type().is_dir()`, which does not follow symlinks, so a symlinked directory is not listed. Keys: move with `j`/`k`/arrows, enter with `l`/Right/Enter (Enter on a directory with no subdirectories confirms it), go up with `h`/Left/Backspace, confirm the current directory with Space, filter with `/`, cancel with Esc or `q`. There is no typed-path entry.
- **Form** (`NewPaneFormState`, rendered by `render_new_pane_form`). A read-only `Dir:` line; the Mode row (`[No mode]`, each `[[modes]]`, each `[Orch: <name>]`, `[schedule]`, `[schedule: issues]` when the experimental flag is on, `[dispatcher]`); the `[auto]` Agent chip over `agent_registry::ALL`, which overwrites Command with the chosen agent's `default_command`; Name, prefilled with the directory basename, switched to the next free `<basename>-orchestrator-N` while an orchestration is selected and the name is untouched, and blocking submit when it equals a live orchestration's title; Command, prefilled from `DashboardConfig.default_command`, then the saved `last_command`, then blank, and hidden while an orchestration is selected.
- **Submit.** A plain agent sends one `AttachRequest::StartAgent` (an empty command means the daemon's default shell). `schedule`, `schedule: issues` and `dispatcher` start a dashboard agent carrying `SCHEDULE_AUTHORING_SEED_PROMPT`, `ISSUE_DISPATCH_AUTHORING_SEED_PROMPT` or `DISPATCHER_SEED_PROMPT`, with a blank Command resolved to the default command and then `claude`; the TUI queues the seed and types it in once the agent is ready. An orchestration writes the coordinator context on the TUI's filesystem and sends one `StartAgent` per role, sharing one freshly minted `orchestration_id`.
- **Afterwards.** A plain or authoring agent: the TUI switches to the Dashboard tab and focuses the new pane (PRD #154). An orchestration: the TUI focuses the start role. A failure is reported in the status bar after the form has closed.

### The deck step

The candidates are the overview's `runtime.fleet` entries, keyed by their wire `deckId` — the value `DeckScope::resolve` accepts. A deck is selectable when its `ConnectionView` is connected and neither pending nor unconfigured, and either compatible or accepted through the overview's existing "Connect anyway". A disabled deck carries the reason the overview already shows for it.

The deck id is **captured once** when the step is confirmed and carried by every later request in the flow — listing, options, resolve, start. None of them reads the applied selection. That is also what keeps `selection_capture.rs`'s per-module budget of raw `trusted_daemon` / `selected_endpoint` reads from growing: the new code paths add no raw reads.

If the chosen deck disconnects mid-flow, the next request fails with `DeckScope::resolve`'s "that deck is not one this app is observing" error, and the flow returns to the deck step with that message rather than retargeting.

### The directory-listing verb

A new `AttachRequest` variant — provisionally `ListDirectories { path: Option<String> }` — advertised under a new capability string. With `path` absent, the daemon lists its starting directory (see [Open Questions](#open-questions)); with a path, it lists that path's immediate subdirectories.

**Reply.** The canonical path that was listed, its parent's canonical path (absent at the root), and one entry per subdirectory: its name, its full canonical path, and whether it holds a `.dot-agent-deck.toml`. The desktop sends back only paths the daemon supplied or the user typed — PRD #819's rule — so it never joins a parent and a child name itself. The project marker is what lets the form decide whether to ask `ResolveProject` for orchestrations; today that verb refuses a directory with no config with a deliberately generic `unresolved` error, which the form would otherwise have to treat as a failure.

**Bounds**, carried from #1048's list:

- **One level per request.** No recursion, no walk.
- **Directories only.** No files, sizes, times, owners or modes.
- **A result cap** with a `truncated` flag, and a time budget, so a huge directory degrades to a partial listing instead of a stalled request.
- **Hidden directories and symlinks** are handled as the TUI's picker handles them (hidden skipped; symlinked directories not listed) unless the open question below decides otherwise, and the listing reuses the existing reader's refusals rather than inventing new ones.
- **Canonical, absolute paths only**, in both directions.

**Threat model — what a caller learns that it could not learn before.** Nothing, for any caller that can reach the attach socket today. The same socket accepts `StartAgent` with an arbitrary command and working directory, executed as the daemon's user — a caller that can send `ListDirectories` can already start `ls -la` anywhere that user can read, and receive its output over `AttachStream`. The verb adds a **structured, bounded** route to information the socket already exposes; it adds no authority. That is the honest answer to #819's "resolve-only, never list" bound: the bound constrained the project verbs, whose purpose was naming projects, and it was never what stopped enumeration.

It is **not** a reason to leave the verb unbounded. #819's audit recorded that bounding the project verbs is not a substitute for authentication if PRD #741 ever admits a peer with less than full account authority, and the same holds here: if such a peer is ever admitted, this verb must be re-examined alongside `StartAgent`, not after it. The bounds above exist for robustness and so that the verb's surface is already small when that day comes.

### What the daemon serves for the form

A second capability-gated query — provisionally `NewAgentOptions {}` — returning what the form needs about the deck rather than the desktop computing it:

- the deck's **default command** (`DashboardConfig.default_command` from the configuration on the daemon's host — the file the TUI reads there);
- the **agent registry** the deck was built with — each agent's display name and default command — so the Agent picker offers what that deck's build knows, not what the desktop's build knows;
- whether the deck's **experimental flag** is on, which decides whether the `schedule: issues` chip is shown — keyed on the deck, because that is where the flag has meaning for the spawn;
- which **authoring kinds** the deck can compose (below).

Orchestrations for a directory come from the existing `ResolveProject`, called only when the listing marks the directory as a project.

### Authoring agents — the seeds move to the daemon

`schedule`, `schedule: issues` and `dispatcher` differ from a plain agent only by a seed prompt, and today those prompts are constants in `src/ui.rs` that the TUI types in after readiness. The desktop must not copy them — a third copy is exactly the drift #1043 describes — and it should not type into the PTY itself either.

So `StartAgent` gains an optional, capability-gated **authoring kind**. The daemon composes the seed from the shared constants (moved out of `src/ui.rs` into a module both use) and delivers it once the agent is ready, through the daemon's existing readiness-gated delivery rather than a new one ([#528](https://github.com/vfarcic/dot-agent-deck/issues/528) tracks unifying the three that exist). The desktop withholds the three chips on a deck that does not advertise the capability. An older daemon ignoring an unknown field would otherwise start the agent with **no seed and no error**, which is why the field is gated rather than merely optional.

### Orchestrations

The launch uses the daemon verbs the desktop already speaks — `PrepareWorkflow { path, orchestration, task, config_revision }`, then `StartPreparedAgent` per role — so the coordinator context is published on the deck, not on the client. Two differences from the Runs screen's use of them:

- **No task prompt.** The TUI's Ctrl+n has none. `PrepareWorkflow` already accepts an empty `task` (the Runs form's refusal is client-side, #1044). What the orchestrator context should say with no task is #1044's own open question; this PRD settles it daemon-side, because an empty `## Your task` heading is worse than omitting the section.
- **The Name field is the run's title**, carried as the `display_title` of each role's orchestration membership, with the TUI's refusal of a title already live on that deck, checked against that deck's snapshot.

After launch, the desktop opens the **start role's** pane, as the TUI focuses it.

### Prefilling Command

The TUI prefills from `default_command`, then its `last_command`, both files on the TUI's host. For the desktop, `default_command` comes from `NewAgentOptions`. Where the desktop's "last command" lives is an [open question](#open-questions); the provisional answer is per deck in `desktop.toml`, keyed by endpoint — never global, for #1048's Part 1 reason that one deck's command means nothing on another.

### After creation

The start action returns the new agent's id through `DeckActionResult`. The desktop does **not** open the pane immediately: `paneAgentRetired` treats a connected deck that does not list the agent as "the agent is gone" and closes the view (`desktop/src/App.tsx`). It waits until the target deck's fleet entry lists `(deckId, agentId)`, then calls `VOICE_ACTIONS.openAgent` with `from: "overview"`.

Two timing facts make that wait worth engineering. `desktop_run_action` ends with `refresh_and_emit` for the **selected** deck only, so a spawn on another deck appears when that deck's watcher next re-fetches. And a spawn emits no broadcast of its own — a `SessionStart` arrives only from the agent's hook — so an agent without hooks appears on the 5 s reconcile. The action should trigger a refresh of the target deck directly, and bound the wait: if the agent is not listed within the bound, the flow closes and the overview says the agent was started but has not appeared.

A failure keeps the dialog open with an inline error and the values the user entered — a deliberate improvement on the TUI's status-bar message, which arrives after the form has already closed.

### Older decks

Each new verb is capability-gated, and the desktop checks the capability in the client library rather than at each call site (the pattern `DaemonClient::focus_gained_while` established for PRD #1105). Against a deck that lacks them:

- **no listing verb** → the directory step offers the typed-path field only; *(superseded: the deck is ineligible, disabled with that reason)*
- **no options query** → Command is prefilled from the desktop's own per-deck memory only, and the Agent picker falls back to the registry compiled into the desktop, labelled as such;
- **no authoring kind** → the `schedule`, `schedule: issues` and `dispatcher` chips are withheld;
- **no `prepare-workflow` capability** → orchestration chips are withheld, carrying the reason the connection already reports as `projectActionsReason` (which the Runs screen's workflow panel shows today).

### Cross-version safety

Every wire addition here is on CLAUDE.md rule 18's no-bump rungs: new request variants that every sender withholds until the daemon advertises them, and an optional field on `StartAgent` gated the same way. `PROTOCOL_VERSION` (10 at `ffcf48d8`) should not move. The semantic question — does any existing field change meaning — is answered explicitly per milestone, and the empty-task composition change in particular: no existing client sends an empty task through `PrepareWorkflow` (the desktop's Runs form refuses one, and the TUI does not use the verb), so its composition change reaches no current caller, but that is a claim to re-verify when the milestone lands, not to inherit from this paragraph. Rule 12's cross-version test runs before the PR (`cargo xver --branch <branch>`); `--direction reverse` is worth running too, since the new verbs live in the daemon.

### Feature flag

Ships **visible** (decided 2026-09-22). The desktop binary has no experimental-flag mechanism — PRD #176's decision 6 and PRD #745's feature-flag section recorded that the flag does not apply to it — and the overview this flow lives on already ships by default. If #1198 later gives the desktop a flag, this flow is not a candidate for it.

### Testing — what rule 4 means here

- **Daemon verbs** — protocol unit tests and socket tests in the root crate for the listing verb's bounds (one level, cap and `truncated`, hidden and symlink handling, refusal of relative paths), the options query, and authoring-kind seed delivery; capability-withheld behaviour for each.
- **Desktop backend** — Rust tests in `desktop/src-tauri` against the existing two-deck `RealDeck` harness: an agent started with **All Decks** selected lands on the chosen, non-local deck; a disconnected deck fails with the resolve error rather than retargeting.
- **Desktop frontend** — vitest suites for the dialog's steps and form rules, and a Playwright spec on the fixture bridge for the whole flow from the overview to the opened pane.
- **Real agent.** Rule 4 asks for at least one test that drives the genuine spawn → agent → work path with a cheap model: an agent started through the deck-targeted path in a browsed directory, asked to report a uniquely named sentinel file. Anything that reaches a real agent is lane 2 and runs on no CI runner (rule 5), so the milestone that adds it also runs it locally with `cargo test-e2e-live <filter>` and names it in the PR.
- **The honest gap.** There is no `tauri-driver` tier ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)), so no automated test drives the real Tauri window. The compensating control is the manual smoke check in `docs/develop/desktop-gui.md`, run against a local deck and a remote one.

## Success Criteria

- From the overview, a user starts a plain agent on any eligible deck, in a directory browsed on that deck that holds no `.dot-agent-deck.toml`, and lands in that agent's pane.
- The same flow starts an orchestration (no task prompt; the Name is the run title), a `schedule` authoring agent, a `schedule: issues` authoring agent where the deck's flag is on, and a dispatcher — each arriving with the same seed text the TUI delivers.
- With **All Decks** selected, the agent starts on the deck the user chose; no action in the flow reads the global selection.
- No path in the flow is derived from the client's environment; linkage-check 12 stays green without an exemption.
- Against an older deck, each missing capability degrades as described in [Older decks](#older-decks) instead of failing silently. *(Amended: a deck without the listing verb is refused at the deck field with its reason rather than falling back to a typed path.)*
- `PROTOCOL_VERSION` is unchanged, and rule 12's cross-version test has been run and recorded.
- #1041 is closed, and #1048 is narrowed to its Part 1.

## Milestones

### Iteration 1 — a plain agent, end to end

- [x] **M1 — The directory-listing verb.** `ListDirectories` in the daemon, capability-gated and bounded as specified, with the threat model recorded in `docs/develop/` and protocol/socket tests for every bound. Closes #1048's Part 2.
- [x] **M2 — The deck's new-agent options.** `NewAgentOptions` (default command, agent registry, experimental state, authoring kinds advertised as none until M7), capability-gated, with tests.
- [x] **M3 — Deck-targeted start in the desktop backend.** A plain-agent start that takes a deck id, resolves it through `DeckScope::resolve`, returns the agent id through `DeckActionResult`, and is reachable from the frontend (`DeckAction`, `TauriDeckBridge.runAction`, the fixture bridge), with `RealDeck` tests including the All Decks case.
- [x] **M4 — The dialog.** Deck step, directory step (browser plus typed path — the typed path was later removed, and the steps were later merged into one dialog), and the form with `No mode`, Agent, Name and Command at TUI parity; entry points on the overview (top bar, shortcut, deck-group header, first-run copy) and the voice registry entry; vitest coverage.
- [x] **M5 — After creation and degradation.** Opens the new agent's pane once the target deck lists it, with a bounded wait and a direct refresh of that deck; inline errors; each older-deck fallback; a Playwright spec for the whole flow.

### Iteration 2 — the rest of the Mode row

- [x] **M6 — Orchestrations.** `[Orch: <name>]` chips from `ResolveProject`, the run-title Name rules, launch through `PrepareWorkflow` / `StartPreparedAgent` with no task prompt, the daemon's orchestrator context composed honestly without a task, and the start role's pane opened afterwards.
- [x] **M7 — Authoring agents.** The seed constants moved to a shared module, the capability-gated authoring kind on `StartAgent` with daemon-side readiness-gated delivery, and the `schedule`, `schedule: issues` and `dispatcher` chips, each verified to deliver the same text the TUI does.

### Iteration 3 — verified and documented

- [x] **M8 — Real agent, docs, cross-version.** The lane-2 real-agent scenario run locally and named; `docs/develop/desktop-gui.md` and the protocol notes updated (and a user-facing page if [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) has given the desktop one by then); a changelog fragment; rule 12's cross-version test run and recorded here; the manual smoke check against a local and a remote deck; #1041 closed and #1048 narrowed.

## Risks

- **Re-opening a recorded decision.** The listing verb contradicts #819's written bound. Mitigated by arguing it here rather than assuming it — the threat model above — and by the bounds and capability gate. If review rejects the argument, the fallback is the typed-path field alone, which weakens parity but not correctness. *(Superseded: review accepted the argument, and the typed-path field was later removed for usability — so the listing verb is now the only way to choose a directory, and a deck without it cannot be used by this flow.)*
- **Seed delivery for agents other than Pi.** Readiness detection varies by agent and has a history of races ([#699](https://github.com/vfarcic/dot-agent-deck/issues/699), [#529](https://github.com/vfarcic/dot-agent-deck/issues/529)). Reusing the daemon's existing delivery path rather than writing a fourth is the mitigation; M7's tests must cover a late readiness announcement.
- **Opening the pane too early closes it.** `paneAgentRetired` closes a view whose agent the deck does not list. The wait-for-fleet step exists for this; its bound must be long enough for a hookless agent's 5 s reconcile.
- **Scope.** Full parity across four spawn kinds on a new UI surface is large. The iterations are ordered so a plain agent ships end to end before orchestrations and authoring kinds are started.

## Open Questions

1. **Where does the directory step start?** The TUI starts at its process's current directory, which under `connect` is the remote login directory. Candidates for the daemon: the daemon user's home directory, or the daemon's startup directory. Provisional: home.
2. **Symlinked and hidden directories.** Mirror the TUI's picker (skip both), or list them? Following a symlink needs a policy consistent with the existing reader's refusals.
3. **Where does the desktop's "last command" live?** Per deck in `desktop.toml` (provisional), or on the deck, shared with the TUI's `session.toml` — which the TUI process writes today.
4. **Keyboard shortcut.** Ctrl+N mirrors the TUI; check it against the desktop's existing bindings and the platform's own (Cmd+N on macOS).
5. **Voice.** `voice: true` opening the dialog at the deck step, or a `no_voice` reason for now ([PRD #1195](https://github.com/vfarcic/dot-agent-deck/issues/1195) is widening the voice command set).

## Related

- [#1041](https://github.com/vfarcic/dot-agent-deck/issues/1041) — `start_agent` has no UI. Closed by this PRD.
- [#1048](https://github.com/vfarcic/dot-agent-deck/issues/1048) — Part 2 (the daemon-side browser) is absorbed here; Part 1 (remembering a project per deck) stays open.
- [#1044](https://github.com/vfarcic/dot-agent-deck/issues/1044), [#1083](https://github.com/vfarcic/dot-agent-deck/issues/1083) — the Runs screen's divergences, revisited separately.
- [#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199) — removing workspace modes, why modes are excluded.
- [#1043](https://github.com/vfarcic/dot-agent-deck/issues/1043), [#555](https://github.com/vfarcic/dot-agent-deck/issues/555), [#528](https://github.com/vfarcic/dot-agent-deck/issues/528) — daemon-owned facts, title uniqueness, prompt-delivery unification.
- [#1196](https://github.com/vfarcic/dot-agent-deck/issues/1196), [#1197](https://github.com/vfarcic/dot-agent-deck/issues/1197), [#1198](https://github.com/vfarcic/dot-agent-deck/issues/1198) — the overview as landing screen, the rail, the flag. Adjacent, not dependencies.
- PRDs [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) (the overview), [#1105](https://github.com/vfarcic/dot-agent-deck/issues/1105) (the pane overlay and cross-deck attach), [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) (the fleet view), [#819](https://github.com/vfarcic/dot-agent-deck/issues/819) (project resolution behind the daemon).

## Work Log

### 2026-09-22 — Created

Scope settled with the user before writing:

- **Parity target**: the TUI's Ctrl+n — plain agent, orchestrations, `schedule`, `schedule: issues`, `dispatcher` — **excluding workspace modes**, which are being removed (#1199). Explicitly **not** based on the Runs screen's workflow creation, which is revisited later.
- **Deck first**: the flow opens on a deck-selection step, because the desktop drives several decks.
- **Directory**: a **daemon-side browser**. Checked whether the daemon half already existed: at `ffcf48d8` no directory-listing verb exists in `src/daemon_protocol.rs` (whose `ResolveProject` doc still says it is not `ListDir`/`ReadFile`/`Stat`), no branch or PR implements one, and the open issue asking for it is #1048's Part 2 — absorbed here.
- **Feature flag**: ships visible (CLAUDE.md rule 9 asked). The desktop binary has no flag mechanism.

### 2026-09-22 — Implemented on `agent/dispatch-prd-1223`

All eight milestones landed. `PROTOCOL_VERSION` stayed **10**: every wire addition is a capability-gated variant or a gated optional field, checked in the client library, so no `.breaking.md` and no `CONTRACT_BREAKS` entry is owed.

| Addition | Capability | Client-library gate |
| --- | --- | --- |
| `AttachRequest::ListDirectories { path? }` | `list-directories` | `DaemonClient::list_directories` |
| `AttachRequest::NewAgentOptions {}` | `new-agent-options` | `DaemonClient::new_agent_options` |
| `StartAgent.authoring_kind` | `authoring-kind` | `DaemonClient::start_authoring_agent` (fresh handshake per call) |
| `StartPreparedAgent.use_configured_command` | `prepared-role-command` (Unix, like `start-prepared-agent`) | `DaemonClient::start_prepared_role` (fresh handshake per call) |

**Open Questions, as settled.** (1) The directory step starts in the daemon user's home. (2) Hidden and symlinked children are skipped as the TUI's picker skips them; a typed path is canonicalised by the daemon. (3) The desktop's last command is kept **in memory** per deck, not in `desktop.toml` — `desktop_settings_secrets`' `ALLOWED_FIELD_TYPES` forbids free text in that file, and a command line is where secrets live; persisting it on the deck is #1048 Part 1. (4) Ctrl+N / Cmd+N, on the overview only; no collision found, unverified on Windows WebView2. (5) `no_voice`, deferring a voice entry to PRD #1195.

**Two findings changed the plan.** The daemon already omitted the `## Your task` section for an empty task and already carried a run title through `StartPreparedAgent.tab_membership`, so M6 needed no new daemon work for either — `project/launch/004` pins both as a regression guard. But role commands deliberately never reach a client (`ProjectRole`), so the desktop could not start roles at TUI parity: M6 added the explicit opt-in `use_configured_command` rather than giving an absent `command` a new meaning, which keeps every existing field's meaning intact.

**A Pi coordinator is allowed here, and refused on the Runs screen.** Runs promises acknowledged all-or-nothing delivery and Pi's native seed cannot be acknowledged; this flow lets the deck seed a Pi start role exactly as the TUI does, and therefore cannot roll back on a Pi delivery failure it cannot detect.

**Tests.** New catalog entries `newagent/browse/001–003`, `newagent/options/001`, `newagent/authoring/001–003`, `newagent/live/001` [reel], `project/launch/004–005`, plus desktop `RealDeck`, vitest and Playwright suites. `newagent/live/001` is the rule 4 real-agent scenario: a real interactive Haiku agent started through the flow's own daemon sequence in a browsed directory, reporting a sentinel file, run locally (lane 2 runs on no CI runner) and recorded for the demo reel.

**Rule 12's cross-version check** was run repeatedly as the branch grew; see the later Work Log entry for the state that actually ships. An earlier version of this paragraph claimed the evidence covered every daemon change, which was wrong: the recorded runs predate the stop-surfacing broadcast. Forward (old daemon, branch TUI) and reverse (branch daemon, old TUI and CLI, `--probe generic`): one `Attach protocol listening` line, the same daemon end to end, the delegate routed, both hook kinds arrived, and the reverse run's `role-set` held. Evidence in the branch's `.dot-agent-deck/xver-prd1223-{forward,reverse}.md`.

**Review and audit.** Four rounds. The audit's accepted findings produced: a fresh capability handshake per gated-field send; control characters, Unicode line separators and bidi formatting characters kept out of listed paths and authoring seeds; a re-check of each listed child at reply construction; a dedicated refusing permit pool so the new queries cannot starve the project verbs; a bounded config read; exactly one valid `DOT_AGENT_DECK_PANE_ID` required on both new surfaces (folded the way the target platform folds env keys); time limits on every role start and rollback stop with indeterminate failures reported rather than assumed; and the cleanup warning made visible and unlosable on both screens. Two findings were declined as pre-existing and cross-cutting — the single fixed `orchestrator-context.md` path per project, and prepared starts spawning by pathname after an inode check — and are tracked in [#1233](https://github.com/vfarcic/dot-agent-deck/issues/1233) with the daemon-side duplicate-name refusal and a daemon-owned preparation deadline. One low residual of the single-latest-error policy is [#1234](https://github.com/vfarcic/dot-agent-deck/issues/1234).

**Not verified by this run.** The manual desktop smoke check against a local and a remote deck (`docs/develop/desktop-gui.md`) needs a human at the GUI; there is no `tauri-driver` tier ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)). No user-facing docs page was added: [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) is still open and the desktop has no page under `docs/`.

### 2026-09-23 — Parity is the default, and the scope bullet that said otherwise

The user, on finding `default_dir` described as the desktop's setting although it lives in the deck's own config: *"That's wrong in the PRD. TUI and desktop should have parity, when that makes sense."*

The Out-of-Scope bullet read "Changing the TUI's picker or form", which was meant narrowly — the TUI keeps reading its own filesystem instead of the new listing verb, because it always runs on the daemon's host — but was written as a blanket exclusion and was read that way while implementing. That produced a deck-level setting only one client honoured: `default_dir` is a `DashboardConfig` key on the deck, yet the TUI's picker still opened at its process's current directory, and the documentation papered over it with "The TUI's directory picker does not read it yet."

The bullet is corrected above, and the principle is recorded rather than left implicit: **a fact the deck owns belongs to every client that can use it, and the TUI and the desktop should have parity wherever parity makes sense.** A difference between them needs a reason rooted in what actually differs — the desktop may be on another machine, the TUI never is — not in which client happened to be built first.

Fixed here: the TUI's Ctrl+n picker now honours `default_dir` with the same vetting the daemon applies (`usable_default_dir`, called directly — the TUI shares the crate), falling back to its current directory exactly as before. The Scheduled Tasks manager's Add opens the same picker and honours it too; its Edit keeps starting at the row's own `working_dir`, the directory that schedule already runs in. Doing it in this PR rather than a follow-up is the cheap moment: the key is new, so nobody has it set, and no existing behaviour changes.

Asymmetries deliberately left, each with its reason: voice exists only on the desktop (the TUI has no voice surface); the desktop's per-deck last command is in memory while the TUI's `last_command` persists (the desktop's settings file forbids free text — see the entry above); and the TUI reads its own filesystem rather than the listing verb, which is the corrected bullet's real content.

### 2026-09-22 — Reshaped after the user tried it: one dialog, closing, live visibility, and voice

The PR reached a merge gate, the user ran the real flow, and the feedback changed the design. Everything in this entry is later than the entry above and supersedes it where they differ.

**The standing principle that drove most of it**, in the user's words: *"All the new features we add must be available through voice control. Otherwise, I'm afraid we'll design it in a way that voice control is not feasible. Good example is what we're doing here. Having multiple 'next' steps to create agent(s) is probably not a good idea when controlled by voice and it would probably make more sense that everything is in the same dialog."* It is now written down in [`docs/develop/voice-first-design.md`](../docs/develop/voice-first-design.md), with this flow as the worked example — a wizard shipped, then collapsed.

**What changed, and why**

- **The typed-path field is gone.** It required an absolute path, which is tedious, and its main justification was a deck too old to advertise `list-directories` — which stops mattering while the desktop is pre-release. A deck that cannot browse is now **ineligible at the deck field** with that reason, rather than selectable into a dead end. The cost is that hidden directories, symlinked directories and anything past the 1000-entry cap are unreachable from the desktop: [#1240](https://github.com/vfarcic/dot-agent-deck/issues/1240).
- **Cancel and the Up button are gone** — each duplicated another control (the X, and the `..` row), and *Back* next to *Up* read as two ways to go back.
- **The three steps became one dialog.** Every field is mounted at once, the browser is a panel rather than a screen, and the step-dependent behaviours were re-homed rather than dropped. This is what makes the surface voice-addressable: a field can be named directly instead of existing only after the step before it.
- **Closing exists now.** *"If one is using desktop, creating agents without being able to close them makes it a no-go."* A stop control on each agent's row, a Close on an orchestration card that stops every role concurrently and names any whose stop could not be confirmed, both behind the app's existing confirmation. `StopAgent` also became **deck-scoped**, closing an asymmetry this PRD had created: you could start an agent on another deck under All Decks and then not stop it.
- **A desktop action is now visible live in an attached TUI.** The user reported that a desktop-started agent did not appear until the TUI was touched, and an orchestration never got its tab — and suspected the desktop and the TUI were two implementations of spawning. They are not: both converge on the daemon's single `StartAgent` arm. The gap was that a TUI-owned start also creates the TUI's own local UI state, while another client's start published nothing. The daemon now emits the surfacing signals the dispatch path already used — a synthetic `SessionStart` or a one-role `OrchestrationSurface` on a start, and a `SessionEnd` carrying a daemon-authoritative marker on a stop. Pinned by `newagent/visibility/001–004`. This reaches beyond the desktop: a Ctrl+W in one TUI now updates another, and the desktop notices a TUI-initiated stop at once instead of on its 5 s reconcile.
- **A per-deck default directory**, served by the deck beside `default_command`. Not in desktop settings: that file's `ALLOWED_FIELD_TYPES` gate forbids free text, which is also why the per-deck last command stays in memory. Editing it means the deck's config today; a settings UI needs a daemon write verb.
- **Voice, for the whole flow.** `deck_ref` and `dir_ref` resolver kinds (the latter resolving against the children **on screen**, which is what makes it possible without the directory-search verb this PRD declined); optional params; a `requires` column, because a dialog is not a `DeckView`; rows for opening the dialog, browsing (`open dir`, `go to parent`, `use this directory`), and filling Mode, Agent and Name. **Command stays manual by design** — it is the field that executes, and a mis-dictated shell command is the one mistake with consequences beyond the form.
- **PRD #802's D5, honoured without building its mechanism.** No spoken command starts or stops anything: "start it", "stop the tester" and "close the review orchestration" each open the existing `ConfirmDialog`, which a human answers. The `confirm` table column #802 anticipates is still #802's to build.

**What the security audit changed.** Four rounds. Accepted and fixed: a fresh capability handshake per gated-field send; control characters, Unicode line separators and bidi formatting characters kept out of listed paths and authoring seeds; a re-check of each listed child at reply construction; a dedicated refusing permit pool so the new queries cannot starve the project verbs; exactly one valid `DOT_AGENT_DECK_PANE_ID` on both new surfaces; bounded role starts and rollback stops with indeterminate failures reported rather than assumed; and, on the voice surface — untrusted names moved out of the system prompt into a data turn, **transcript grounding** for every resolved reference, **action grounding** so a hostile label cannot steer the model into a row the user did not ask for, **whole-utterance** grounding for `submit_prompt` because it is irreversible, and a privacy disclosure narrowed until it was true (three attempts; the first two made request-wide claims the always-sent transcript falsifies). Names can be withheld entirely with `[voice] labels = "withheld"`, at the cost of the commands that name things.

**Declined, with reasons, and tracked**: the shared fixed `orchestrator-context.md` path per project and the pathname-based cwd after an inode check — both pre-existing and cross-cutting — plus `PrepareWorkflow`'s first-match rule for duplicate names and a daemon-owned preparation deadline ([#1233](https://github.com/vfarcic/dot-agent-deck/issues/1233)); an unconfirmed-stop warning that can be replaced before it is shown ([#1234](https://github.com/vfarcic/dot-agent-deck/issues/1234)); a bare "go ahead" not submitting ([#1246](https://github.com/vfarcic/dot-agent-deck/issues/1246)); and a late or forged `SessionStart` re-registering a closed pane, which is the unauthenticated raw-event admission already tracked in #543/#401.

**Cross-version (rule 12), verified rather than argued.** An earlier version of this Work Log claimed the evidence covered every daemon change; it did not, and the reviewer caught it. Both directions pass against `v0.41.1`, and — because `--probe generic` exercises neither — a **targeted manual reverse run** covered the two paths that actually changed: an external `StopAgent` leaves a previous-release TUI's card as an ordinary ended card (the same stale card it kept before, when no event was sent at all), its own close still works, and a daemon-internal scheduled spawn renders **exactly one** card, because the new `session_id` matches the shape of that TUI's own placeholder key. Nothing is worse; one thing is better. No `.breaking.md` and no `CONTRACT_BREAKS` entry. `PROTOCOL_VERSION` remains **10**, and no root-crate `src/` file changed after the evidence was gathered.

**Still not verified by machine**: the manual desktop smoke check against a local and a remote deck ([`docs/develop/desktop-gui.md`](../docs/develop/desktop-gui.md)), because there is no `tauri-driver` tier ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)); and the voice phrase fixtures, which are credentialed and run on no CI runner. The published demo-reel clip still demonstrates the real-agent path honestly, but it predates the dialog, voice and close controls and shows none of them.

### 2026-09-23 — The Agent field removed from both clients; two voice papercuts

**The Agent field is gone, in the TUI and the desktop, at the user's decision.** The TUI chip sat outside the Tab cycle, so a keyboard user could not reach it at all; the one test of it, the L2 `prompt/new-pane/015`, reached it by clicking; `auto` meant nothing; its label went stale the moment Command was edited; and it saved one word of typing, because every agent's `default_command` is the bare binary name. Removed in the TUI: `FormField::Agent`, `agent_selection`, `agent_label`, `select_agent`, the two cyclers, the key and click arms, the render block and its height reservation, with an L1 test (`new_pane_form_has_no_agent_row`) pinning the absence and the Mode → Name → Command Tab cycle, and `prompt/new-pane/015` — which cycled the chip — rewritten to pin the absence in the real binary. Removed in the desktop: the `<select>`, `AUTO_AGENT`, `agentChoice` and its reset, `chooseAgent`, the `formSignature` member and the "The list is this app's own." hint, with the dialog tests and the Playwright focus-order spec updated to the shorter tab order.

**Voice keeps a way to choose the agent.** `choose_agent_type` ("use claude") now sets Command to that agent's `default_command`, resolved against the deck's own registry (or the desktop's fallback copy for a deck that reports none), and reports `Command set to Claude Code's default command.` — overwriting what is there, and refused while an orchestration is selected, whose roles run their own commands. Without it hands-free use would have no way to choose what the agent runs, since Command is never dictated. `auto` left the voice vocabulary with the picker. No wire change: `NewAgentOptions.agents` and the fallback registry stay, for this row.

**Two voice defects from the user's first real use.** "Select directory code" was refused as `nothing in that asks for "open_dir"`: `open_dir`'s vocabulary lacked `select`, `choose`, `pick`, `go to` and `navigate` (added, with a description sentence naming them), and the refusal quoted an internal id. Every row now carries `asks_to` and `try_saying`, so the sentence reads *nothing in that asks to open a directory, so nothing was done; try “open code”* — the placeholder filled only with words the user said. The New agent dialog's unavailable hints now say what to do first (*say “new agent” and choose a deck first*; *choose those first*), since its fields fill in order. Phrase fixtures, 76 of 76 against the default backend after each part, including two for the reported phrasing.

### 2026-09-23 — An optional param that fails is dropped, not a refusal of the action

**"Create a new agent" was refused** as *you did not name “Local deck”, so nothing was done*: the model filled `open_new_agent`'s optional `deck` with the local deck, and reference grounding correctly found nothing in the transcript supporting it — then refused the whole action. Grounding exists to stop an unsupported value being acted on, and for an optional param leaving it out is exactly the safe outcome: the same state as the user not supplying it, which the row already handles, while the action itself is held against the transcript separately. So an optional param that fails is now **dropped** and the row dispatches without it, whichever way it failed — not grounded, matching no deck, matching several, or withheld by Settings → Voice → Names — and the report says so rather than dropping it silently: *Opening the New agent dialog. I did not catch which deck, so none is preselected.* A value the user really did say is named back with what stopped it (*No deck matches “ghost box”, so none is preselected.*; *“build” matches more than one deck, so none is preselected: …*), which is why the ambiguous case drops too — the candidates are on screen and the dialog it opens is where the deck is chosen. A required param that fails still refuses. Phrase fixtures, 77 of 77 in each of two runs against the default backend, including a new `open-new-agent-create` for the reported phrasing; gpt-5-mini did not fill the deck for it in either run, so the fixture pins the outcome and the unit test is what reproduces the defect.

### 2026-09-23 — "Close" never stops; a spoken start starts; a start with the dialog closed opens it

Three changes the user asked for after using the shipped voice flow, plus the vocabulary gaps the row walk found. All the description changes below are **prompt changes** — a row's `description` is what the model picks on.

**D1 — "close the agent" closes the view.** The user said *"Close the agent"* and it was taken as stopping the agent. The rule, now a named principle in [`docs/develop/voice-first-design.md`](../docs/develop/voice-first-design.md) section 5: *an ambiguous reading resolves to the non-destructive action, and a destructive action needs unambiguous wording.* `close`'s description claims "close the agent" and its screen/view/pane synonyms and says the agent keeps running; `stop_agent`'s says "close" is not a stop word (its `heard_as` never had one, and gained "shut it down"); `schema::TOOL_INSTRUCTIONS` states the rule once for every row. `close_orchestration` needed more than a description: its verbs are the words a view is closed with, so "close the billing agent" grounded it on `close`. It now needs the orchestration or the run **named** as well — a conjunction `heard_as` could not express, since its entries are alternatives — through a new `heard_as_also` column (`orchestration`, `run`, `every role`, `all roles`, `all the roles`), with `try_saying` now "close the {orchestration} orchestration". Grounding on the nouns alone was rejected because it would have dropped the verb requirement. The two stops keep their confirmations. Measured before the change, gpt-5-mini routed "Close the agent" to `close` in the baseline run on both the agent screen and the overview, so the model-side misroute did not reproduce on demand; the pipeline defect did (`voice_outcome_close_the_agent_never_grounds_closing_an_orchestration` dispatched a `close_orchestration` for "close the billing agent" before the change).

**D2 — "start it" starts, with no confirmation.** The user's words: *"it shows another popup asking me to confirm. I think that only introduced friction by me having to give the same instruction twice."* PRD #802's **D5 is revisited for the start only**, recorded in [#802's D5](802-desktop-voice-control.md#milestones), in `voice-first-design.md` section 5, and here. The reasons: **action grounding** now requires the transcript to contain a start word, so a hostile label can no longer steer the model into starting something while the user said something else — the injection risk D5 was written against is closed at a different layer; the confirmation **re-stated what is already on screen**; and a mistaken agent is now **one "close" away**. What stays: `stop_agent` and `close_orchestration` are destructive, their target may be off-screen with no form to read, and an orchestration close takes several roles at once. The row's `invoke` is now `startNewAgent`, which calls the function the Start button calls; the confirmation's "the form changed" signature check was replaced by the same deck/directory re-check the fill rows use (`FORM_MOVED_ON`). Pins moved deliberately: `voice_table_d5_rows_only_ask` now holds the two stops and asserts the start row no longer claims to ask; the vitest D5 block's start tests were rewritten from "opens a confirmation and starts nothing" to "starts at once"; `START_AWAITING_CONFIRMATION`, `START_FORM_CHANGED` and `FORM_UNDER_CONFIRMATION` went with the confirmation.

**D3 — a start with the dialog closed opens it.** *"Start the new agent"* with no dialog open got *"Not here — … say 'new agent' first."* `start_new_agent`'s old *"pick it whenever the user asks to start, however urgently"* was written for a row that only asked, and it overrode the prompt's callable tie-break; removing it and naming the case in `open_new_agent`'s description was enough for "Start the new agent" (the model answered `open_new_agent` in every run after). It was **not** enough for a bare "start it", which the model answered with `start_new_agent` in both runs — including one whose description said in so many words not to pick it while `callable: false`, and which also cost the adversarial start fixture, so that sentence was reverted. So the table gained `unavailable_opens`: a pick of `start_new_agent` that cannot run dispatches `open_new_agent` when that row can run here and the same words ground it. It is decided by the table rather than left to the model because the tie-break is a probability and the target only opens a dialog. The optional-deck fix landed just before this interacts correctly: the redirect dispatches `open_new_agent` with no params, which is the "no deck named" case that fix made safe.

**D4 — vocabulary gaps.** `open_new_agent` hears `spawn` and `another` ("spawn an agent", "I want another agent"). `close` hears `cancel` and `never mind`, and over the New agent dialog also "cancel", "cancel this", "cancel it", "never mind" and "close the new agent dialog" — the dialog has no Cancel button any more (U2), so voice is the only place that word can land. They are **base** `heard_as` words, not only whole-utterance entries, because the parser lets a context list only narrow `heard_as`; that widens `close` everywhere else too, which costs nothing there, since it closes a pane or the command list and either reopens as it was. `open_deck` over the dialog accepts "show me the deck" and "show the deck". **`go_to_parent` on a bare "go back" is still refused, by decision rather than omission**: "back" belongs to `close` and `open_deck`, and a user filling the form who says "go back" is as likely to mean either.

**Tests, each seen failing first.** Rust: `voice_outcome_close_the_agent_never_grounds_closing_an_orchestration`, `voice_outcome_start_it_starts_without_asking`, `voice_outcome_a_start_picked_with_the_dialog_closed_opens_it` (red with the exact sentence the user got), and `voice_outcome_the_row_walk_gaps_are_heard` (all seven phrasings refused before); table pins `voice_table_conjunctive_rows_are_the_deliberate_set`, `voice_table_redirecting_rows_are_the_deliberate_set` and `voice_table_close_the_agent_grounds_only_the_view`, plus parser tests for both new columns. Vitest: four rewritten start tests in `VoiceControlCommands.test.tsx`, red against the previous source.

**Phrase fixtures, against gpt-5-mini.** Fourteen new ones for the user's phrasings ("Close the agent" on two screens, "close the agent screen", "stop the agent", "stop the tester agent", "close the build orchestration", "Start the new agent" with the dialog open and closed, "spawn an agent", "I want another agent", "cancel", "never mind", "close the new agent dialog", "show me the deck"), and `start-new-agent-dialog-closed` re-pinned from `unavailable` to opening the dialog. Before the change: **83 of 91**, every failure one of the new targets. On the final table: **90 of 91** and **91 of 91**. The one miss (`open-dir-dialog-closed`, an untouched row, answered `none`) passed in every other run; so did the one-off miss of the intermediate table (`open-deck-unavailable`).
