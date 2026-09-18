# PRD #802: Voice control for the desktop app

**Status**: In progress — this document was written first and is what implementation is delegated against
**Priority**: Medium (its hard dependency, [PRD #803](803-desktop-settings-surface.md), has shipped; nothing is blocked on this)
**Created**: 2026-09-18

## Problem Statement

The desktop app ([#176](https://github.com/vfarcic/dot-agent-deck/issues/176)) currently mirrors the TUI, and for a terminal user that is strictly worse than the TUI: it costs a window and returns nothing. The test a second front-end has to pass is whether it does something the first **structurally cannot**, and two things do.

[#742](https://github.com/vfarcic/dot-agent-deck/issues/742) (fleet view) is one: a TUI is one terminal attached to one daemon — `Endpoint` (`src/daemon_client.rs:68`) is a single `Local`/`Remote` choice and not a map — while the desktop already holds many at once, one `TrustedDaemon` per deck in `links: AsyncMutex<HashMap<EndpointIdentity, Arc<TrustedDaemon>>>` (`desktop/src-tauri/src/daemon_bridge.rs:284`). #742's own body calls this "the strongest case for a GUI at all".

**Voice is the other.** It needs a microphone, OS integration and a persistent local process — awkward through `ssh -t`, natural in a native app.

Together they give the desktop app a job of its own, and it is not the TUI's job:

| | job |
| --- | --- |
| **TUI** | the **working** surface — one repo, dense, keyboard-driven, `ssh`-able; you are *in* it with an agent |
| **Desktop** | the **supervisory** surface — many daemons, attention routing, cost, voice; you are deciding *where to look* |

Voice is the input method that fits supervision, because a supervisor is not typing.

The second problem this solves is smaller and more immediate: **the user should never have to guess the phrasing.** "Get rid of everything except the tester", "show me the one that's stuck" and "zoom the tester" have to resolve the same way, which a keyword grammar cannot do and a model mapping an utterance against live state can.

## Solution Overview

Six steps, and the fifth is the one that keeps this from becoming a second control surface.

1. **Capture** — a microphone, or typed text into the same box.
2. **Transcribe** — speech to text, producing a plain string.
3. **Resolve intent** — one model call carrying the command table, the live state the app already has, and the transcript.
4. **Validate** — the app checks the returned action exists, is callable on the current screen, and that its parameters resolve against live state.
5. **Execute** — hand the resolved action to the **existing** handler. Voice gets no execution path of its own; it produces an action and dispatches it where a click dispatches one.
6. **Report** — say what happened, showing what was heard when nothing matched.

**The command table is the whole design**, and its acceptance criterion is stated as a test rather than an aspiration: **after the plumbing exists, voice-enabling a new feature is a ONE-FILE change.** Implement the feature as normal, then add a row. A row **does not define behaviour — it exposes behaviour that already exists**, so the table is a reference and not a second implementation.

Three consumers read that one file, which is what keeps it one file: **schema generation** (the model's tool definition), **validation** (`screens` and each param `kind`), and **dispatch** (`invoke` names an existing action). A guard in `cargo test-fast` keeps the stringly-typed references honest, because a typo in `invoke` would otherwise surface at runtime as *nothing happening*.

**What this PR ships is the spine and one activation mode, with both model stages behind seams and no bundled native engine.** The reasoning is in [Staging](#staging-what-this-pr-ships-and-why-the-issues-m1-was-split) and it is a deliberate departure from the issue's M1, recorded rather than implied.

## Scope

### In Scope

- **The command table**, as one TOML file embedded with `include_str!`, plus its three consumers: generated schema, validation, dispatch resolution.
- **A single frontend action registry** that the rail, the command palette and the voice dispatcher all dispatch through, so step 5 above is literally true and the guard has something enumerable to check `invoke` against.
- **The pipeline end to end** — capture, transcribe, resolve, validate, execute, report — with `Transcriber` and `IntentResolver` as seams chosen in the #803 settings.
- **An intent backend that works with no API key and no download**, using the pre-authenticated agent CLI the user already has on the box, plus a keyed remote backend as the quality upgrade.
- **The `[voice]` settings section** and the `SecretStore` that #803 M5 named and deliberately did not build, because this PRD is the first consumer that needs one.
- **One activation mode** — press once to start, press once to stop.
- **Navigation-only commands**, so the first slice cannot misfire expensively, with at least three rows plus a deliberate no-match so disambiguation and the "none of these" path are both exercised.
- **The structural guard**, as a `linkage-check` rule: `invoke` names a registered action, every `screens` entry is a known screen, every param `kind` is in the closed set, and every capability in the registry has either a table row or an explicit `no_voice` marker.
- **The phrase fixtures**, in the credentialed lane, opt-in and skipped when no backend is configured.
- **The one-file proof** — a milestone whose whole content is adding the second command as a table-and-fixtures edit, which fails if anything else has to be touched.

### Out of Scope

- **Any bundled native inference engine.** No `whisper-rs`, no `llama-cpp-2`, no model files. See [Dependency cost](#dependency-cost-and-the-one-native-dependency-this-pr-does-take) for why this is a staging decision about `desktop/src-tauri` being a workspace member rather than a change of direction.
- **Model download management** — progress, retry, disk-usage visibility, removal. With no bundled engine in this PR, nothing downloads; this arrives with the local engines that need it.
- **The remaining two activation modes**, hold-to-talk and always-on with voice-activity detection.
- **Destructive commands.** Approve/deny a permission prompt, close an agent's **terminal pane**, stop an agent — none of them gets a row here; they are D5, behind a confirmation. **"Close a pane" is one word away in speech from something that IS in scope, so name both operations rather than leaving the next reader to infer the split.** Closing an agent's *terminal pane* destroys a PTY and stops work: lifecycle, destructive, deferred. Dismissing the agent *view* — the screen overlaying the deck — is `closeAgent` in `desktop/src/App.tsx`, which is `setView({ kind: from })` and nothing else: it destroys nothing and stops nothing, so it is navigation in exactly the sense the other three rows are, and it ships here as `close_agent_view`. The hazard is real because [#1105](https://github.com/vfarcic/dot-agent-deck/issues/1105) calls the overlay "the agent pane", so the same word points at both sides of this line. Two things follow from keeping the row beyond the classification: without it the `agent` screen has **no callable command at all**, which is a functional hole rather than a coverage one, and the `callable: true` path on that screen would be exercised by nothing. Its `description` says in as many words that it does not stop the agent and does not close its terminal pane, and that sentence is doing real work on the model.
- **Modal dictation** ("type to the tester" until "finished with that agent").
- **Discovery** — contextual examples rendered from the table.
- **Chaining.** The table knows a command's prerequisite, so the app can *offer* the next step; performing that chain automatically is deliberately absent, because a single utterance silently performing two operations is where a misfire gets expensive.
- **An audio-native single-hop backend** doing transcription and intent together. Cloud-only and streams the microphone continuously; an option later, never the default.
- **Siri and any OS-native speech API.** The issue's reasoning is unchanged and is restated in [Transcription](#transcription-and-the-one-place-there-is-no-no-key-trick).
- **The TUI↔desktop parity matrix.** Floated in the issue under "Explicitly rejected" as the enforceable alternative to an unenforceable rule; a real piece of work of its own, deferred with a reason rather than dropped.
- **Any daemon protocol change.** See [Cross-version safety](#cross-version-safety).
- **A driver-level test tier for the desktop app.** None exists ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)) and this PRD does not build one.

## Technical Approach

### What was measured, and where

Everything in this section was read or run against this branch at **`d7bbbd39`** on **2026-09-18**, on Linux. Where a claim could not be verified it says so, which is the point of saying where the rest came from. Two claims were measured by running something rather than by reading it — the webview capture matrix and the agent-CLI intent backend — and both name their method inline.

### Staging: what this PR ships, and why the issue's M1 was split

The issue's M1 is one milestone containing capture, whisper STT, the table, the three consumers, validation, dispatch, the guard, the fixtures and one activation mode. Two things in it carry a cost this repo feels immediately, and the split is about those two and nothing else.

**`desktop/src-tauri` is a workspace member** (`Cargo.toml`'s `[workspace] members`), so anything it depends on is built by `cargo test-fast --workspace` — the per-task gate every contributor runs — and by all three required build jobs (`build`, `build-windows`, `build-macos`). Bundling whisper.cpp via `whisper-rs` or llama.cpp via `llama-cpp-2` therefore lands a cross-platform C/C++ build toolchain in that gate, on a matrix that includes Windows. That is a repo-wide change to what it costs to run the tests, and it is not a change this feature has to make in order to exist.

So: **the two engines sit behind seams — a `Transcriber` and an `IntentResolver`, chosen in the #803 settings — and no bundled native engine ships in this PR.** Local whisper and a local grammar-constrained intent model become their own deferred milestones, and the seams exist precisely so they drop in without touching the spine. The issue's direction is not being overturned; its *first* milestone is being made deliverable.

**For intent resolution there is a default that needs no key and no download, and it is this product's own asset rather than a trick.** The user already has a pre-authenticated agent CLI on the box — `claude`, `opencode` — which the daemon spawns routinely. A non-interactive print-mode call to that CLI is a candidate default intent backend. [It was measured](#intent-resolution-the-no-key-default-measured) and it works, with a latency and a cost that have to be stated rather than discovered.

**Transcription has no equivalent trick, and this PRD says so plainly.** Every no-download transcription option needs a credential. So **when no transcription backend is configured the surface still works, from typed input, through the identical resolve → validate → execute → report path**, and the settings panel says what to add. The spine is exercisable, nothing is presented as broken, and local whisper is the milestone that removes the requirement rather than a gap being hidden.

### The command table

One file, `desktop/src-tauri/src/voice/commands.toml`, embedded with `include_str!` so it ships inside the binary and cannot drift from the build that compiled it. It lives in the Rust crate because two of its three consumers are Rust-side and the third reaches it through an existing IPC reply rather than through a second copy.

```toml
[[commands]]
id          = "open_agent"
description = """Open one agent's pane over the current screen. Use when the user asks
to focus on, enlarge, zoom, or see only one agent."""
invoke      = "openAgent"               # an entry in VOICE_ACTIONS
screens     = ["deck", "overview"]      # absent or empty = available everywhere
unavailable_hint = "opening an agent works from the deck or the overview"
report      = "Opening {agent}."        # the sentence shown on a SUCCESSFUL dispatch

  [[commands.params]]
  name = "agent"
  kind = "agent_ref"                    # one of a small closed set of resolver kinds
```

**The `description` column is a prompt, not documentation.** The model picks on it, so it is written for a model and reviewed as an interface.

**The `report` column is the success sentence, and without it the "the app renders every sentence" property is lost.** Every *refusal* has somewhere to get its wording — `unavailable_hint` for the not-here case, fixed prose in `outcome.rs` for the rest — and success had nowhere, so the surface would have had to invent success wording in the frontend. A `{param}` placeholder is replaced with what that param resolved to, and a placeholder naming no declared param is refused at parse time: that is the same stringly-reference discipline `invoke` gets from the guard, applied to the one column carrying a reference inside itself.

**It is called `report`, not `confirmation`, and the name is load-bearing.** [D5](#milestones) is *destructive commands behind confirmation* — an actual confirm-before-acting dialog — so rows carrying a genuine confirmation flag will live in this same file. The two are close to opposite: a report describes a completed action, a confirmation blocks one before it happens. The name also matches the pipeline's own stage 6, **Report**. `commands.toml` carries a one-line comment at the column saying so, because the rename is otherwise easy to undo.

The three consumers:

1. **Schema generation** — `id`, `description` and `params` become the model's tool definition.
2. **Validation** — `screens` and each param `kind` check the model's answer before anything runs.
3. **Dispatch** — `invoke` names the action to run, and the app calls it with the resolved params; `report` is the sentence it shows afterwards.

### Dispatch: the `invoke` column names a FRONTEND action, and the issue's wording is wrong about this

The issue says `invoke` names "an existing `#[tauri::command]`". **The survey falsifies that for exactly the commands the issue's own example uses, and this is the single most consequential correction in this document.**

The Tauri command registry at HEAD is one `tauri::generate_handler!` block, `desktop/src-tauri/src/lib.rs:2174-2188`, holding **thirteen** entries: `desktop_get_snapshot`, `desktop_list_projects`, `desktop_resolve_project`, `desktop_bootstrap`, `desktop_terminal_attach`, `desktop_terminal_write`, `desktop_terminal_resize`, `desktop_terminal_detach`, `desktop_get_settings`, `desktop_set_settings`, `desktop_test_endpoint`, `desktop_set_zoom`, `desktop_run_action`. There are exactly thirteen `#[tauri::command]` attributes under `desktop/src-tauri/src/`, all in `lib.rs`, so the registered set and the defined set agree today — but that is an observation about today, not a property, and the guard must read the registration block rather than the attributes for that reason.

**None of those thirteen performs a navigation**, and that was checked variant by variant rather than by reading their names: the widest of them, `desktop_run_action`, takes a `DesktopAction` (`desktop/src-tauri/src/dto.rs:500-565`, twelve variants) whose variants are `Refresh`, `Bootstrap`, `StartAgent`, `StartWorkflow`, `StopAgent`, `StopDaemon`, `RestartDaemon`, `AllowBuildMismatch`, `RenameAgent`, `AttachTerminal`, `DetachTerminal` and `SubmitText` — every one a daemon, agent-lifecycle or terminal operation, and not one a screen change. Navigating to the overview is `onNavigate?.({ kind: "overview" })` (`desktop/src/App.tsx:1063`); opening an agent's pane is a `DeckView` variant set in React state (`desktop/src/types.ts:321`, switched at `desktop/src/App.tsx:357-375`); opening a settings or prompts sheet is a `useState` boolean. `desktop_set_zoom` is the *webview's* zoom level, not "zoom an agent's pane". So a table whose `invoke` column named a Tauri command could expose the daemon-facing verbs and could not expose a single one of the navigation commands this PR is scoped to.

**The resolution, and it is the issue's own step 5 taken literally.** Voice "produces an action and dispatches it where a click dispatches one" — and a click is dispatched in the frontend. So `invoke` names an entry in **one frontend action registry**, `desktop/src/lib/voiceActions.ts`, exporting `VOICE_ACTIONS` as an object literal keyed by action id. A registry entry whose behaviour is Rust-side is a one-line `invoke("desktop_…")` call inside it, so the registry stays the single dispatch seam rather than becoming a second one beside the Tauri registry.

**How a guard enumerates each side.** Both are text scans, which is the established idiom here rather than a compromise: `xtask/linkage-check/src/desktop_settings_secrets.rs` already scans `desktop/src/lib/bridge.ts` for a declared interface's fields and for `localStorage` key expressions, from Rust, in a required gate.

- **The Tauri registry** is enumerable by a text scan of the `generate_handler!` block: find `.invoke_handler(tauri::generate_handler![`, read to the matching `])`, split on commas, trim. One identifier per line today. The `#[tauri::command]` macro does generate a `__cmd__<name>` item, but nothing in the source spells that out, so it is not what a scan reads — and reading the attributes instead would accept a command that is defined and *not registered*, which is precisely the silent failure the guard exists to catch. A scan of the registration site is sufficient **and** is the right one.
- **The frontend registry** is enumerable the same way: the keys of the `VOICE_ACTIONS` object literal. This is only sound while the registry is a literal with statically-readable keys, so the guard also refuses a computed key or a spread inside it — the same refusal `desktop_settings_secrets.rs` applies to a spread in `normalizeDesktopSettings`, and for the same reason.

### One tool with an `action` enum, and the full list every time

With a tool-per-command schema, *listing* a tool means the model may call it, so a per-entry availability flag is advisory at best. **A single tool taking `action` as an enum** keeps the output constrained — the model cannot invent an action — while letting every entry carry an availability flag.

Every command is sent on every request, each annotated `callable: true|false` computed from `screens` against the current screen, rather than the list being filtered. Filtering produces a bad failure: asking to open an agent from a screen where that is impossible yields "I don't know how to do that", which is false. The truthful answer names the prerequisite, and while people are learning the app that case is more common than a genuinely unsupported request. One round trip, including on the failure path, which is where a second one hurts most.

**The trade, stated because it is easy to forget:** filtering *prevents* wrong calls; flagging *explains* them. Flagging relies entirely on validation to refuse an action the model called anyway. That is acceptable because validation exists regardless — but the flag improves the message, not the misfire rate.

### The model returns a situation; the app renders the sentence

The model returns structure — `{"action": "open_agent", "callable": false}` — and the app renders the user-facing sentence from the table. Three reasons: wording stays consistent instead of varying per utterance; a fixture test can assert an action id, whereas asserting on free-form prose is miserable; and the model cannot invent a plausible-sounding but wrong reason for why something is unavailable, because `screens` already knows.

**The model must be able to answer "none of these".** Forcing a pick from a closed set turns "what time is it?" into a command. That one affordance is most of the safety, and [it was measured to work](#intent-resolution-the-no-key-default-measured).

**A failure must say what it heard** — "Heard: *'go beck'* — no matching action." Most failures are transcription rather than intent, and showing the transcript turns a dead end into a correction.

Concretely, the Rust side returns one closed-set outcome per utterance, each carrying its own rendered sentence. The frontend dispatches on the first and renders the sentence the outcome carries. Rust renders the sentences because Rust holds the table; the frontend renders no voice prose of its own.

**There are nine of them, not four, and the four this document first listed cannot express the other five.** M1 found that out by building them: a missing param, a param that resolves to nothing, a param that resolves to more than one thing, a backend that could not answer at all, and a backend that named an action outside the table are five genuinely distinct situations, and each of the first four needs a different sentence from the user's point of view. Collapsing them into *no matching action* would tell a user "I don't know how to do that" when the truth is "I could not tell which agent you meant" or "nothing configured yet".

| outcome | what happened | carries |
| --- | --- | --- |
| **Dispatch** | run this action with these params — the one outcome that asks for anything to run | transcript, action, `invoke`, resolved params, the row's `report` |
| **Unavailable** | the action exists and this screen cannot run it | transcript, action, the row's `unavailable_hint` |
| **NoMatch** | the model answered "none of these" | transcript |
| **UnknownAction** | the model named an action that is not in the table | transcript, the action it named |
| **ParamMissing** | the action declares a param and the model supplied none | transcript, action, param |
| **ParamUnresolved** | a param was supplied and nothing in live state matches it | transcript, action, param, what was spoken |
| **ParamAmbiguous** | a param was supplied and more than one thing matches it | transcript, action, param, what was spoken, the matches |
| **ResolutionFailed** | the intent backend could not answer — unconfigured, timed out, unparseable | transcript, detail |
| **TranscriptionFailed** | speech could not be turned into text; upstream of everything else, so the one outcome with no transcript | detail |

**`UnknownAction` and `NoMatch` deliberately render the SAME sentence while staying separate variants, and that is not a redundancy to simplify away.** The *cause* differs and the *effect* does not: a model that invents an action id is a backend not honouring the enum — impossible under grammar-constrained decoding, merely unlikely under the print-mode agent CLI — while a no-match is the escape working exactly as designed. From where the user is standing both mean the app did not know how to do what they asked, so both say so. Keeping them apart is what lets a fixture, a log or a future metric tell a misbehaving backend from a genuine no-match; merging them would throw that away to save one variant.

### Screens: the smallest honest closed set is three

"The current screen" has a precise meaning in this app and it is narrower than it sounds. `DeckView` (`desktop/src/types.ts:297-321`) is a discriminated union with exactly three variants — `{ kind: "deck" }`, `{ kind: "overview" }` and `{ kind: "agent"; deckId; agentId; from }` — and `DeckShell` switches on it at `desktop/src/App.tsx:357-375`, over a `base` derived at `:182`. There is no router library and none is wanted (`types.ts:291-296` says so).

**So the closed set for the `screens` column is `deck`, `overview`, `agent`, and the guard checks an entry against the `kind` literals of that union in `types.ts`.** A text scan again, and it keeps the table and the app's own type in step without a second list.

**What that set deliberately cannot express, and this is worth knowing before writing a row.** Five of the rail's seven buttons toggle overlay booleans inside `ControlDeck` (`desktop/src/App.tsx:1060-1067`) rather than changing the view, and #803's Settings sheet is one of them. Those booleans are not in `DeckView`, so *"an overlay is open"* is not part of the closed set and a command's availability cannot depend on it without first moving that state somewhere a screen identifier can see. No row in this PR needs that. A row that does need it is a change to where the state lives, and it should be recognised as that rather than worked around in the table.

### Capabilities: the mechanical definition, and exactly what it costs

The guard is required to assert that every user-facing desktop capability has **either** a table row **or** an explicit `no_voice` marker. That assertion is only implementable if "user-facing desktop capability" has a mechanical definition, and the honest answer has two halves.

**The definition this PRD adopts: a capability is an entry in `VOICE_ACTIONS`** — the same single registry `invoke` dispatches through. Each entry carries `id`, a human label, and either a table row somewhere or `no_voice: "<reason>"`. The guard then asserts a total function from registry to classification, which is a real assertion: it cannot be satisfied by forgetting, only by writing a reason.

**What makes it more than an honour system is that the registry is load-bearing at runtime.** It is not a list beside the app; it is what the rail buttons and the command palette dispatch through, so an entry cannot be deleted without breaking a control, and a control reachable through the palette cannot exist without an entry. That is the property that earns the definition, and it is why **M2 moves the palette's existing `commandItems` array (`desktop/src/App.tsx:1043-1053`, seven static entries plus per-agent and fixture-mode ones) into the registry.** Without that move the registry is a parallel list with no consumer, which is the checked-in-list failure under a better name.

**And here is the cost, stated plainly rather than implied: the guard cannot discover a capability on its own.** A control wired with a bare `onClick` that does not route through the registry is invisible to it. Measured at `d7bbbd39`: **80 `onClick=` sites in non-test `.tsx` under `desktop/src`**, against 7 static palette entries and 7 rail buttons. Most of the ones sampled while writing this are chrome — dismiss a toast, close a sheet, tick a column — but all 80 were not classified, and the guard could not tell a capability from a close button anyway, so this document does not claim the residue is harmless. So the claim the guard supports is narrower than the issue's wording and is the one to write into the code: **every entry in the action registry is classified, and the registry is the app's own dispatch path for rail and palette controls.** Completeness of the registry against the whole rendered surface is not mechanically discoverable, and the one mechanical approximation — pinning the `onClick` count so a new interactive control forces a classification — is [Open Question 3](#open-questions) with its churn cost named, not something this PRD adopts quietly.

### The guard: where it goes, and what number it takes

`xtask/linkage-check` has two shapes and the difference matters for where this lands.

Most desktop-adjacent rules there are `#[cfg(test)]` modules whose whole content is `#[test]` functions — `desktop_palette`, `desktop_settings_secrets`, `contract_breaks`, `gh_aw_lock_consistency` and a dozen others (`xtask/linkage-check/src/main.rs:100-200`). They run under `cargo test-fast --workspace` (CLAUDE.md rule 5's `--workspace` is what reaches them) and therefore in the CI `build` job, one of the four required checks. The other shape is a **numbered live rule** in the binary's own run; there are twelve, and `desktop_project_boundary` is check 12 — the one desktop module carrying a live rule as well as its own tests. `cargo xtask linkage-check` runs in the same `build` job (`.github/workflows/ci.yml:512`).

**This guard takes both shapes, deliberately, and they are not redundant.**

- The **table-integrity half** — `invoke` names a registry entry, `screens` entries are known screens, param `kind` is in the closed set, every registry entry is classified — becomes **numbered rule 13**, because it is a cross-file consistency rule of exactly the kind the numbered rules are for, its failure output belongs in the per-finding summary, and rule 7's precedent is that a rule CI runs is a rule that gets fixed. The numbers are stable identifiers in the failure output, so this takes 13 rather than renumbering anything.
- A `#[cfg(test)]` module beside it holds the **scanner's own tests**, in the idiom `desktop_settings_secrets.rs` establishes: a test proving the scanner catches a planted bad `invoke`, a planted unknown screen and a planted unclassified registry entry, so "no findings" is a meaningful result rather than a vacuous one. Skipping this is how a guard ends up scanning nothing and passing.

Same budget as its neighbours: reads files, no network, no git, no sleep, no subprocess, and a filesystem error fails the test rather than silently shrinking the scan.

**Why not a typed Rust enum**, which the compiler would check: adding a command would then mean editing the table *and* a variant *and* a match arm — three places, and the one-file goal is gone. Reference-plus-guard buys the same safety at commit time, which is the trade `tests/CATALOG.md` already makes.

### The settings section, and the credential rule that binds this PRD hardest

#803 shipped the store and the contract, and it left this PRD one obligation that is easy to miss and impossible to work around.

**The contract itself is three steps** (`docs/develop/desktop-gui.md:1002-1009`): a field or a section struct on `DesktopSettings` in `desktop/src-tauri/src/settings.rs`; one row — `id`, `label`, `icon`, `component` — on `SETTINGS_SECTIONS` in `desktop/src/lib/settingsRegistry.ts` plus your own panel implementing `SettingsPanelProps` from `desktop/src/lib/settingsContract.ts`; and never a secret in the document. The registry already names this PRD as an expected tenant in its own doc comment. Three sections exist today: `appearance`, `decks`, `zoom`. The document is `platform::paths::config_dir().join("desktop.toml")`, overridable by `DOT_AGENT_DECK_DESKTOP_CONFIG`.

Two constraints from that contract apply directly. Field names stay `snake_case` **and single-word where that is natural**, because the same struct is serialised to TOML for the user and to JSON for the webview and every name today is one word; the first genuinely multi-word field is the point at which a separate webview DTO has to be introduced. And a section whose frontend half has not landed is declared `Option<…>`, because `#[serde(default)]` turns "the webview said nothing" into "the webview said empty" and the merge then writes that empty section over the user's data — `endpoints` is the worked example. A `[voice]` section that ships with its panel in the same PR does not need the `Option`, but it does need the frontend to round-trip it from the moment the panel can render it.

**The part that binds hardest: `xtask/linkage-check/src/desktop_settings_secrets.rs` will go RED when this PRD adds a `String`, and it says so in its own module docs** — *"the one that will go red when PRD #802 adds a `String`, which is exactly the moment somebody has to route it through the `SecretStore` seam instead."* That is not a guess about the guard; it is the guard's stated purpose.

Its four checks, read rather than assumed:

1. **Every field in the Rust settings schema has a type from a pinned allowlist**, `ALLOWED_FIELD_TYPES`, currently **fifteen** entries: the scalars `u32`, `SshPort`, `AppearanceMode`, `ZoomLevel`, `Selection`, the five ssh-argument newtypes `Hostname`, `SshUser`, `KeyPath`, `HostAlias`, `RemoteSocketPath`, and `EndpointId`; plus four section structs whose own fields the scan then walks. A field is resolved through `allowlisted_as`, which strips one `Option<…>` or `Vec<…>` layer — so `Option<String>` and `Vec<String>` resolve to `String` and have no way in either. **`String` is not on the list. Neither is `bool`, `PathBuf`, or any map.** A serde attribute that hides a field from the serialised document (`skip`, `flatten`) is refused for the same reason: the sentinel sweep in `settings.rs` walks that document, so a field missing from it is a field nothing follows a value through.
2. **The TypeScript DTO's declared shape is pinned field by field with its types** — `PINNED_TS_FIELDS`, sixteen rows today, over `DesktopSettingsDto` and friends in `desktop/src/lib/bridge.ts`. The Rust check cannot see `bridge.ts` at all, and a frontend-only `apiKey` once got declared there where nothing read it.
3. **`normalizeDesktopSettings` constructs its result and never copies its input** — a spread anywhere in that function is refused, so an extra field arriving over IPC is dropped rather than carried.
4. **The `localStorage` key set is pinned** — `PINNED_STORAGE_KEYS`, five entries, every access must name one of those constants, and `localStorage` may only be spelled `localStorage.<op>(...)` so an alias cannot name a key nothing can read. Its stated scope is `localStorage` in `.ts`/`.tsx` under `desktop/src`; `sessionStorage`, IndexedDB, `document.cookie` and `.js` files are outside it, none of the four appears under `desktop/src` today, and the moment one does it needs its own row.

**So the settings work this PRD owes is concrete:**

- A `[voice]` section whose every field is an allowlisted type. The backend choices, the activation mode and any model identifier are **closed enums** with folding deserializers in the `AppearanceMode` idiom — not free strings — which satisfies the guard by being the right shape rather than by an exemption. The app only supports the backends it ships an adapter for, so a closed enum is also the truthful type.
- Any genuinely new type — including `bool`, if a plain on/off field is wanted — is a **deliberate edit to `ALLOWED_FIELD_TYPES` with a written reason in the entry**, which is exactly the friction that list exists to apply. Do not reach for an exemption; state what values the type can be.
- **Building the `SecretStore`.** #803 M5 named the seam — store, load, delete, keyed by a stable identifier, OS keychain as the intended implementation via `keyring` — and deliberately did not build it, because an unused trait is dead code and #802 would design its shape against a real backend. This is that moment. The document may hold a non-secret reference (which backend holds a key, or whether one is stored) and nothing more. Two things #803 recorded so this PRD would not rediscover them: a Linux box with no Secret Service needs a documented, non-silent failure path, and an owner-only file in the config directory is the viable fallback if one is wanted. "Read it from the environment" is not available to an app launched from Finder.
- **There is no credential storage anywhere in this repo to copy** — no crate here *declares* `keyring`, `secret-service`, `security-framework` or `stronghold`, and what exists is credential *detection* in the e2e harness, which reads and never writes. So `keyring` is a genuinely new dependency in a workspace member, and it belongs in the [dependency accounting](#dependency-cost-and-the-one-native-dependency-this-pr-does-take) with the rest. (**M4 narrowed that sentence**, which said the four were at "zero matches workspace-wide". True of declarations and not of the lockfile: `security-framework 3.7.0` is already in it, reaching the graph on macOS through `reqwest` → `rustls-platform-verifier`. The dependency is still new; the Apple backend is a second consumer of a crate already there.)
- Extending `PINNED_TS_FIELDS` and `normalizeDesktopSettings` in the same commit as the Rust section, because check 2 fails otherwise and that is the check doing its job.

### The CSP, and why every network hop is Rust-side

Verified at `d7bbbd39`. `desktop/src-tauri/tauri.conf.json:26` carries:

```
default-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'self' asset: http://asset.localhost data:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'
```

`connect-src` names `ipc:` and `http://ipc.localhost` and nothing else, so **the frontend has no way to make a request to a network origin and read the reply** — `fetch`, `XMLHttpRequest`, `WebSocket` and `EventSource` are all governed by `connect-src` and all refused. The consequence for this feature is simplifying rather than awkward: **every network hop — transcription, intent resolution, any future model download — is Rust-side.** #803 already reached the same conclusion for downloads and put it in its Out of Scope for this reason.

The narrower claim is deliberate: *"the frontend cannot make a network call at all"* is how this is usually stated and it is one quantifier too wide. `form-action` has no `default-src` fallback and is not declared here, so a form submission or a top-level navigation is not what `connect-src` refuses. That is not a route to reading a transcription reply, which is why the conclusion above is unaffected — but it is the kind of sentence CLAUDE.md rule 17 exists to catch, and writing the wide version here would put it in a document read as a specification.

`script-src 'self'` has a second consequence worth writing down, because it decides a detail in [capture](#capture-what-each-webview-can-actually-do-measured): a `blob:` URL is not `'self'`, so an `AudioWorklet` module loaded from a blob would be refused in the packaged app. A worklet, if one is used, ships as a real file in the bundle. The `devCsp` beside it adds only `http://localhost:1420` and `ws://localhost:1420` for the dev server, so it changes nothing here.

**Current Tauri surface, because a microphone and any plugin are a capability change rather than a toggle.** `tauri = { version = "2", features = [] }` in `desktop/src-tauri/Cargo.toml` — no features — and the capability set is `"permissions": ["core:default"]` for the `main` window, the only entry in `desktop/src-tauri/capabilities/` (`default.json`). So there is no Tauri plugin enabled at all, and #803 already rejected `tauri-plugin-store` partly on that ground. Resolved crate versions: `tauri 2.11.5`, `wry 0.55.1`, `tao 0.35.3`. `bundle.active` is `false` in the base config and `true` only in the `desktop/src-tauri/tauri.bundle.conf.json` overlay the release job passes — which is also where a macOS `Info.plist` addition would have to live, and where `release.yml` already merges the real version with `jq`.

### Capture: what each webview can actually do, measured

This is the question the issue does not answer and the one most likely to be assumed wrong, so it was measured three ways.

**In Playwright's two engines**, launched from `~/.cache/ms-playwright` on 2026-09-18, against a secure `https` origin fulfilled by a route handler:

| engine | `grantPermissions(["microphone"])` | `getUserMedia({audio:true})` | `MediaRecorder` |
| --- | --- | --- | --- |
| Chromium 153.0.8010.12, no flags | accepted | **threw `NotFoundError: Requested device not found`** | present; `audio/webm`, `audio/webm;codecs=opus`, `audio/mp4` supported, `audio/ogg;codecs=opus` and `audio/wav` not |
| Chromium, `--use-fake-device-for-media-stream --use-fake-ui-for-media-stream` | accepted | **1 track, label `"Fake Default Audio Input"`** | recorded one 2340-byte chunk in 600 ms |
| WebKit 26.6, no flags | accepted | **1 track, label `"Mock audio device 1"`** — a mock device with no flags at all | **absent — `MediaRecorder` is `undefined`** |

Two things follow. **A Playwright spec can fake a microphone** — in Chromium with two launch flags, and in Playwright's WebKit with none. And **a `MediaRecorder`-based capture path cannot be exercised in the WebKit project at all**, which matters far more than the test tier, because WebKit is the engine this app *ships* on.

**The Web Audio route is available in both**, measured the same way: `AudioContext({sampleRate: 16000})` honoured the rate in both engines, and `createMediaStreamSource`, `AudioWorkletNode`, `ctx.audioWorklet` and `createScriptProcessor` are all present in both. Chromium loaded a worklet module from a `blob:` URL and delivered 128-frame buffers; WebKit refused the blob module (`'text/html' is not a valid JavaScript MIME type for module script`), which is consistent with the CSP conclusion above that a worklet must ship as a bundled file rather than a blob. Raw PCM is also what whisper wants, so it is the better wire format anyway — no container, no codec, no decode step in Rust.

**Caveat, because it changes how much these numbers are worth.** `desktop/`'s own `@playwright/test` is not installed in this worktree, so the probe was driven by a `playwright-core` from an unrelated project against the browsers in the shared cache. The engine versions are therefore not guaranteed to be the ones `desktop/playwright.config.ts` launches in CI, and the config's own comment already states the wider limit: Playwright's WebKit is its own build from the WebKit source tree (`minibrowser-gtk` on Linux), **not** the distribution's WebKitGTK, not WKWebView, and not a Tauri window.

**In the webviews the app actually ships on, the answer is per-platform and one platform is blocked.** Read from `wry 0.55.1` in the cargo registry, since that is what the build resolves:

| platform | what `wry` does | consequence |
| --- | --- | --- |
| **macOS, WKWebView** | implements `webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:` and calls it with `WKPermissionDecision::Grant` unconditionally (`src/wkwebview/class/wry_web_view_ui_delegate.rs:126-136`) | the webview layer will not block it; the OS layer still gates it, and an `.app` with no `NSMicrophoneUsageDescription` does not get the microphone |
| **Windows, WebView2** | adds a `PermissionRequested` handler **only when `attributes.clipboard` is set**, and allows only `COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ` (`src/webview2/mod.rs:498-515`) | nothing in `wry` decides the microphone kind, so it falls to WebView2's own default handling. Not verified — there is no Windows host here — and Windows is not a desktop bundle target today (`release.yml`'s matrix comment says so) |
| **Linux, WebKitGTK** | **nothing.** A case-insensitive grep for `permission` across the whole crate matches only the two files above; the WebKitGTK backend sets `enable_webgl`, `enable_webaudio`, page cache and developer extras (`src/webkitgtk/mod.rs:427-449`) and never `enable-media-stream`, and it connects no `permission-request` signal | with `WebKitSettings:enable-media-stream` left at its default and no permission-request handler, webview-side `getUserMedia` on Linux has no path to being granted without an upstream change. This is read from the source rather than observed in a running window — stated as evidence, not as a run |

**So webview-side capture would ship a macOS-only feature**, which is the same shape of mistake the issue already rejects when it argues for whisper over `SFSpeechRecognizer`: build it once for three platforms rather than twice for one. **The recommendation is therefore to capture Rust-side**, in the desktop crate, and hand PCM to the transcription seam. That also removes the worklet, the CSP question and the three-webview matrix in one move, and it is where the audio has to end up anyway because the transcription hop is Rust-side by CSP. Its cost is a native audio dependency, accounted for below.

The residual macOS obligation does not go away with Rust-side capture: a bundled `.app` recording audio needs `NSMicrophoneUsageDescription`, and the place for it is the `bundle` section of `desktop/src-tauri/tauri.bundle.conf.json` (Tauri v2 carries Info.plist additions under the macOS bundle config). Whether the dev-time `tauri dev` window on macOS needs the same entry to avoid a silent denial is **not verified here** and is [Open Question 2](#open-questions).

### Intent resolution: the no-key default, measured

Measured on 2026-09-18 on this box with `claude 2.1.277`, three runs of `claude -p --model claude-haiku-4-5 --output-format json` against a closed action set and a short utterance.

**It works.** "show me just the tester" against `["zoom_agent","go_overview","none"]` returned `{"action":"zoom_agent","pane_ref":"tester"}`. "what time is it" against a four-entry set returned `{"action":"none"}` twice — so the **"none of these" affordance, which the issue calls most of the safety, holds on this backend** rather than being assumed.

**Four costs, all of which have to be written down rather than discovered.**

- **Latency: 4.49–4.68 s wall** across three runs, with `duration_api_ms` 3137–3202 and time-to-first-token ~3.1 s. That is a poor interactive experience for a supervisor pressing a button, and it is the strongest argument for the seam: a direct keyed API call or a local grammar-constrained model both replace this backend without touching the spine.
- **Cost: $0.0036–$0.0126 per utterance**, and the variance is the tell — it is driven by `cache_creation_input_tokens: 7695` plus `cache_read_input_tokens: 13790` for a **ten-token** user input. Print mode loads the CLI's own session context on every call, so the price is the harness rather than the task. The cold run paid $0.0126, a warm one $0.0036.
- **The output is not constrained.** Every one of the three runs came back wrapped in a ```` ```json ```` fence despite the prompt forbidding one, so the caller strips a fence and then parses, and a malformed reply is an ordinary outcome to handle rather than an impossibility. This is the concrete difference from the issue's GBNF direction: grammar-constrained decoding makes an invalid action unrepresentable, and this backend makes it merely unlikely. Validation is what refuses it either way, which is why this is tolerable as a default.
- **Stdout carries banner noise.** The first run printed a warning about `ANTHROPIC_API_KEY` taking precedence over a claude.ai login *before* the JSON. So the reader must locate the JSON rather than trusting that stdout is JSON — the same problem `src/login_shell.rs` solves for its PATH probe with marker tokens, and the same lesson applies.

**"No key" needs narrowing, and the narrowed claim is still the interesting one.** The CLI is pre-authenticated with whatever the *user* gave it — on this box that is an environment variable — so it is not that no credential exists anywhere. What is true, and what makes this worth being the default, is that **the app needs no credential of its own and asks the user for nothing**: the thing the issue is emphatic about, that a feature needing credentials before it does anything is a feature most people never try.

**Two mechanisms this needs already exist in the tree.** `src/login_shell.rs` captures the user's interactive-login-shell PATH and applies it to the process environment (`capture_login_shell_path`, `apply_login_shell_path`), which is exactly the problem an app launched from Finder has when it wants to find `claude` in `~/.local/bin`; it is `pub`, it is in the root crate the desktop crate links, and its only caller today is `src/main.rs:1459` on the daemon path. And `src/agent_registry.rs:361-362` already carries the CLI's identity — `detect_basenames: &["claude"]`, `default_command: Some("claude")`. So this backend reuses PATH resolution rather than inventing it.

**And there is a closer precedent than expected, which came out of checking an absolute rather than trusting it.** The sentence that wanted to go here was *"nothing in `src/` invokes an agent CLI non-interactively — every agent goes through a PTY"*, and it is **false**: `src/codex_hooks_manage.rs:640` spawns `codex app-server` with piped stdio to list installed hooks. It is not inference, so the spirit of the claim survives — no agent is *asked anything* outside a PTY — but the mechanical precedent is real and is exactly the right shape to copy: `CODEX_HOME` pinned on the child, stderr discarded, the wait bounded by a named timeout constant, the child killed before returning, and **every** failure mode (binary absent, non-zero exit, protocol drift, timeout) returned as `Err` so the caller degrades quietly rather than blocking. An intent backend that spawns an agent CLI needs each of those five, and it should read that function before writing them again. For print mode specifically, `docs/develop/config-gen-regeneration.md:49` already documents a `claude -p --model claude-haiku-4-5` invocation.

`opencode` offers `opencode run [message..]` and `opencode serve`, so the same shape exists for the other CLI. Not measured; the adapter is written against whichever CLI the settings name, and the second one is best-effort until someone runs it.

**The honest verdict: workable as a zero-configuration default that proves the spine, needs no key and no download, and is slow.** It is what makes the feature try-able on day one; it is not the end state, and the PRD should not pretend 4.5 seconds is fine. A keyed remote backend ships beside it in the same milestone for anyone who wants the latency to be tolerable.

### Transcription, and the one place there is no no-key trick

**Siri is the wrong tool**, and the issue's reasoning stands: it does not hand an app a transcript, it matches speech against *declared* App Intents — a structured vocabulary committed to in advance, which is precisely the guess-the-phrasing problem this PRD exists to avoid — it needs native Swift in the bundle with no first-class Tauri path, and it hands matching to Apple's matcher rather than to a model that can be given live daemon state.

**`SFSpeechRecognizer` is the right Apple piece and the wrong foundation**, also unchanged: on-device, no key, no cost, and available on exactly one of three platforms. Windows has its own APIs and Linux has nothing comparable, so a local model is required for Linux regardless, and building whisper covers all three in one implementation. OS-native STT is a later per-platform optimisation.

**And neither is in this PR.** What ships is the `Transcriber` seam plus one remote backend, chosen in settings, with a credential in the `SecretStore`. **When no transcription backend is configured the panel works from typed input** through the identical resolve → validate → execute → report path, and the settings panel says what to add. That is a deliberate product statement and not a degraded mode being dressed up: the whole pipeline downstream of the transducer is exercisable, the failure message is a configuration instruction rather than an error, and local whisper is the deferred milestone that removes the requirement.

**The consequence the issue names for rejecting an exact-match fast path applies here too, and it is worth repeating because it lands on a later milestone rather than this one:** with no zero-download tier, **first run of the local engines must handle model fetching well** — explain what is downloading and why, show progress, and never present voice as broken while it is fetching.

### Phrase fixtures: two halves, in two lanes

The issue puts the guard in `cargo test-fast` and the fixtures beside the table, and those two cannot be the same thing, because a fixture needs a model and `cargo test-fast` must not reach one.

- **The structural guard is deterministic and belongs in `cargo test-fast`** — every `invoke` names a registered action, every `screens` entry is a known screen, every param `kind` is in the closed set, every registry entry has a row or a `no_voice` marker. That is [numbered rule 13](#the-guard-where-it-goes-and-what-number-it-takes) and it reads files only.
- **The phrase fixtures need a real model, so they belong in lane 2** — CLAUDE.md rule 5's `e2e-live` idiom: opt-in, skipped when no backend is configured, run on a developer's machine and nowhere in CI. They live in the desktop crate rather than in `tests/e2e_*.rs`, because what they exercise is the desktop crate's resolver and not the TUI's spawn path; the lane discipline they inherit is the reasoning, not the file location. Set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` when a skip should read as UNVERIFIED rather than green.

**The fixtures are not optional, and the reason is the issue's own:** adding a row can silently steal utterances from an existing one. A new "close the panel" quietly takes utterances from "close the pane"; nothing errors, the model just starts picking differently, and it surfaces weeks later as a misfire. They must also cover availability — a phrase valid on the deck resolves there *and* produces the unavailable message on the overview — because otherwise a `screens` typo makes a command quietly unreachable on the screen it was written for.

**And they are authoritative only against the reference backend.** A fixture that passes with a remote model may fail with a local one, so "adding a row is safe" is only true for whoever's backend matches the reference. The reference is whichever backend the settings default to — in this PR, the agent-CLI one — and other backends get a smaller smoke set that is best-effort. Leaving this implicit would reproduce the hole class [#494](https://github.com/vfarcic/dot-agent-deck/issues/494) removed: a knob whose behaviour nothing verifies.

### Dependency cost, and the one native dependency this PR does take

Stated as an accounting, because the staging decision above is entirely about this.

**Already in the graph, costing nothing new.** `reqwest = { version = "0.13", default-features = false, features = ["rustls", "json"] }` is a root-crate dependency (`Cargo.toml:47-50`), used today only by `src/version.rs`, and the desktop crate path-depends on the root crate — so **an HTTPS client with JSON is already available to `desktop/src-tauri` with no new dependency at all.** `tokio` is there. `toml_edit` with `serde` is there, which is what parses the command table. `serde`/`serde_json` are there. There is **no** audio or inference crate in the lockfile — `cpal`, `rodio`, `whisper-rs`, `llama-cpp-2`, `hound` and `symphonia` all match nothing.

**New, and each one is a decision rather than an incidental.**

- **`keyring`** for the `SecretStore`. Required by #803's rule, no precedent in the repo to reuse, and the Linux Secret Service failure path needs designing rather than assuming. **Resolved at M4 as `keyring` 4.2.0 on default features, and it needs no system dev package on any of the three platforms** — the `v1` feature takes the Security framework on macOS, `windows-sys` on Windows, and a *pure-Rust* zbus Secret Service client on Linux, so the three-file edit below is the audio crate's alone. 59 lockfile entries; 13.9 s wall to build them all from cold on 16 cores.
- **A Rust-side audio capture crate** (`cpal` is the obvious candidate) for [capture](#capture-what-each-webview-can-actually-do-measured). This is the one native dependency this PR takes, and the cost is precise: on Linux it wants ALSA development headers, and **three places would go red without an edit** — `tauri-deps/flake.nix`'s `deps` list, which has `glib`, `gtk3`, `webkitgtk_4_1`, `libsoup_3`, `libayatana-appindicator`, `librsvg`, `xdotool` and `dbus` and no `alsa-lib`; and the two `apt-get install` blocks in `.github/workflows/ci.yml` (`:407-410` for the `build` job and `:629-632` for `e2e-deterministic`), which name the same set and no `libasound2-dev`. macOS needs no dev package for CoreAudio and Windows none for WASAPI, so the edit is Linux-shaped. That is a bounded, statable change to three files, and it is why this dependency is acceptable where a C/C++ inference engine is not: no CMake, no bundled C++ tree, no model, and no new build toolchain — a Rust binding to a system library the platform already has.
- **Nothing else.** In particular no Tauri plugin, so the capability set stays `["core:default"]`.

**The alternative that was rejected, and why**, because it is the tempting one: put the audio crate behind an off-by-default cargo feature so the workspace gate does not build it. That reproduces the exact hole class this repo has closed three times — [#407](https://github.com/vfarcic/dot-agent-deck/issues/407), [#436](https://github.com/vfarcic/dot-agent-deck/issues/436), [#502](https://github.com/vfarcic/dot-agent-deck/issues/502) — where code behind a feature nobody's gate enables is type-checked by nothing and lints clean by compiling nothing. Rule 2's clippy command names `e2e,e2e-live` for precisely this reason. A feature is not how this gets cheaper; deferring the *engines* is.

### Cross-version safety

CLAUDE.md rule 12, answered from the rule's own definition rather than by shape, and CLAUDE.md rule 18's instruction taken seriously: if the desktop needs something the daemon does not expose, the answer is to **add it to the daemon**, not to contort the client.

**Did this change the TUI↔daemon contract? No.** `PROTOCOL_VERSION` stays **10** (`src/daemon_protocol.rs:421`). No `.breaking.md` fragment, no `CONTRACT_BREAKS` entry, and no cross-version manual test is owed.

**The reasoning, because "no" has to be argued here rather than asserted.** The live state the resolver needs is role names, statuses and which pane is focused, plus the current screen — and every piece of it is something the desktop already reads:

- **Role names and statuses** arrive over `AttachRequest::ListAgents` (handled at `src/daemon_protocol.rs:2876-2877`, returning `registry.agent_records()`) and are projected into `DesktopAgent` (`desktop/src-tauri/src/dto.rs:340`), which already carries `status`, `active_tool`, `tool_count`, `cli_name`, `last_user_prompt`, `spawned_at_ms` and `last_activity_ms`. The agent overview renders them today, so the resolver reads a snapshot that already exists rather than requesting anything.
- **The focused pane and the current screen are frontend state.** The selected agent is a `useState` in `ControlDeck`; the screen is `DeckView` in `DeckShell`. Neither is on the wire and neither needs to be.
- **The Tauri commands this PRD adds are webview↔desktop-crate IPC, which is not the daemon wire.** #1105's section of this name settled the wider question and the reasoning is its own: rule 12 defines a break as *"an older and a newer build can no longer safely interoperate"*, and the webview bundle and the Rust binary are built, versioned and shipped as one artifact, so there is no configuration in which an old bundle meets a new binary.
- **Rule 12's trigger is a change to "the daemon, the TUI↔daemon protocol, orchestration, or hooks."** This is a change to a daemon **client** and touches none of the four. The manual test was **not run**, and that is stated rather than implied, because what it exercises — a branch TUI against a previous-release daemon — has no path through this code.

**Where a future vocabulary item would land, so the graded rung is named rather than left to be re-derived.** #745 already recorded that this screen's data ceiling is voice's vocabulary ceiling: "show me the one that's stuck" resolves against a status the daemon reports, and "the one burning the most money" cannot, because cost is not on the wire ([#633](https://github.com/vfarcic/dot-agent-deck/issues/633)). Adding it would be an **additive optional field** on an existing message — `#[serde(default, skip_serializing_if = "Option::is_none")]` — which is the protocol's own written **do-not-bump** case (`src/daemon_protocol.rs:12-18`), and PRD #745 landed two such fields without moving the constant. If voice ever needed a new *verb* from the daemon, the next rung is a **capability-gated new request variant**, also no bump, with the gate in the client library rather than at each call site (#1105's `CAP_FOCUS_GAINED` is the worked example). Only a wire-shape change bumps, and only a same-wire/different-meaning change owes a fragment plus a `CONTRACT_BREAKS` entry. **Reaching for one of those rungs is normal and is not scope creep**; what this PRD asserts is the narrower thing, that the commands it actually ships need none of them.

Patch bump.

### Feature flag

CLAUDE.md rule 9 asks whether a new user-visible surface ships behind `experimental`. **The answer is no.** This was asked of the user while planning this PRD and answered **no**, so: no `src/features.rs` wrapper, no `docs/develop/experimental-flag.md` entry, and no `graduate-voice-control` follow-up issue.

**The issue body's `## Rule 9` section says the opposite and is SUPERSEDED.** It reads "it ships behind the `experimental` flag with a `graduate-voice-control` follow-up at ship time", and that is no longer the decision. It is recorded here explicitly so the next reader does not re-derive the wrong answer from the issue.

The decision also has a precedent rather than being a fresh judgement, which is worth knowing because it is the same answer #745 and #1105 both recorded. `prds/176-desktop-gui.md` decision 6 settled it for the entire desktop binary: the flag is a presentation switch gating render and input seams inside the *TUI* binary, a separate GUI binary has no such seam because building and running it is itself the opt-in, and maturity is handled by packaging. #803's Open Question 1 added the confirming mechanics: the desktop crate links the root library so `features::experimental_enabled()` is callable, but the desktop never calls `features::init_and_watch` — whose only callers are `src/main.rs:1764` and `:2368` — so the flag would read its `false` default forever regardless of TOML or env, and gating this surface would mean building the flag's Tauri delivery mechanism as part of this PRD.

### Testing: what rule 4 means here

Rule 4's vocabulary is the Rust TUI's — L1 is `insta` + `TestBackend`, L2 is PTY + vt100 in `tests/e2e_*.rs` — and this feature lands in the Tauri app, so the mapping is stated rather than assumed. There are **three** desktop tiers and none of them is rule 4's L2. Counted at `d7bbbd39`:

- **Rust unit tests inside `desktop/src-tauri`** — **302** `#[test]`/`#[tokio::test]` functions. They run under `cargo test-fast --workspace` and are linted by rule 2's clippy command, and the `build` job that runs both is one of the four **required** checks. **This is the only desktop tier that blocks a merge, so it is where this feature's substance has to live**: table parsing and its rejections, schema generation, the `callable` computation against each screen, every validation refusal, param resolution against a snapshot, the outcome-to-sentence rendering, and the settings section's round-trip. It also holds the `SecretStore`'s behaviour.
- **vitest + jsdom + Testing Library** — **25 files, 514 `it`/`test` cases**, run by `pnpm test` in the `desktop-web` CI job. jsdom computes no geometry. This covers the panel rendering, the typed-input path, the dispatch through `VOICE_ACTIONS`, and that the palette and rail still dispatch through the registry after M2 moves them. **`desktop-web` is ADVISORY** — the required set is exactly `build`, `build-macos`, `build-windows`, `security` — so a red vitest run does not block a merge, which is why the guard lives in `linkage-check` and not here.
- **Playwright over the built bundle** — **16 spec files, 56 `test()` calls**, in two projects (Chromium and WebKit), run by `pnpm test:browser` in the `desktop-browser` CI job, also **advisory**. Its `webServer` runs `vite build` then `vite preview` and serves `desktop/dist`, and specs drive the app through the fixture query strings (`?fixture=1&state=<scenario>`, scenarios `connected | crowded | empty | disconnected | error | fleet`) with no daemon involved.

**What each tier cannot reach, which is the half that gets assumed wrong.**

- **A Playwright spec cannot exercise a Rust-side Tauri command.** There is no Tauri runtime in that tier at all — it is `vite preview` serving static files — so the IPC an `invoke` would travel over does not exist, and the app selects its `FixtureDeckBridge` rather than `TauriDeckBridge`. Anything Rust-side is out of its reach by construction. **The bridge seam is what makes the browser tier useful here anyway**: a fixture-mode resolver returning a canned outcome lets a spec drive the whole panel, the situation-to-sentence rendering and the dispatch, in two real engines, with no model and no credential.
- **A Playwright spec CAN fake a microphone** — [measured above](#capture-what-each-webview-can-actually-do-measured): Chromium with two launch flags, and Playwright's WebKit with none. So a webview-capture path would have been testable there. With Rust-side capture the relevant tier moves: the capture code is Rust and its tests are Rust unit tests over a stubbed device, and what nothing automated can prove is that a real microphone on a real OS produced audio.
- **Nothing drives the real Tauri window.** There is no `tauri-driver` and no WebDriver session in this repository ([#953](https://github.com/vfarcic/dot-agent-deck/issues/953)), so real IPC, the distribution's own WebKitGTK, WKWebView, and the OS microphone permission dialogs are exercised by nothing automated. **This is not rule-4 parity and is not claimed as such.** The compensating control is the manual smoke check in `docs/develop/desktop-gui.md`, which a milestone extends — and which, like every manual check, decays.

**The three things no automated tier here can prove**, listed so green checks are not read as covering them: that the OS granted the microphone on any platform; that a real utterance transcribes; and that the agent-CLI backend is reachable from an app launched from Finder with a Finder-shaped PATH. All three are manual, and the third is the one most likely to be discovered late — it is [Open Question 1](#open-questions).

## Success Criteria

- **Voice-enabling a new feature is a one-file change**, proved by a milestone that adds the second command by editing the table and its fixtures and nothing else. If anything else has to be touched, the plumbing milestone is not finished.
- A spoken or typed request that maps to a command **executes through the same handler a click executes**, with no second execution path anywhere in the diff — checkable by the action registry having exactly one dispatch site per entry.
- Asking for something that exists but is not available on the current screen produces **the table's own hint naming the prerequisite**, not "I don't know how to do that".
- Asking for something outside the closed set produces a **no-match that shows the transcript**, and the model is able to answer "none of these" rather than being forced to pick.
- The user-facing sentence for every outcome is **rendered by the app from the table**, so the model writes no user-facing prose at all.
- A typo in `invoke`, an unknown screen in `screens`, an unknown param `kind`, or an unclassified capability **fails `cargo test-fast` and the required `build` job**, rather than surfacing at runtime as nothing happening.
- **No credential is in `desktop.toml` or in `localStorage`**, enforced by `desktop_settings_secrets.rs` continuing to pass with the `[voice]` section present — which for a `String` field means the guard going red until it is routed through the `SecretStore`.
- **With no transcription backend configured the surface still works from typed input**, end to end, and the settings panel says what to add. Nothing about it reads as broken.
- **With no API key and nothing downloaded, intent resolution works** on a machine that has an authenticated agent CLI, and the latency is reported to the user rather than looking like a hang.
- The daemon is unaware that any of this exists: `PROTOCOL_VERSION` unchanged, no verb added, no fragment owed.
- `cargo test-fast --workspace` and all three required build jobs stay green on every platform, including the one that needs a new Linux dev package.

## Milestones

**In this PR.** Each is independently deliverable and testable, and they are ordered so the riskiest platform-specific work is last rather than first.

- [x] **M1 — The table and its three consumers, Rust-side.** `commands.toml` with `include_str!`, the parse and its rejections, generated single-tool enum schema, the `callable` computation from `screens` against a given screen, validation of action and params, and resolution of `invoke` to an action id plus params. No model, no capture, no UI: the resolver seam takes a transcript and returns an outcome, and a stub resolver stands in. Rust tests for every rejection and every outcome variant.
- [ ] **M2 — The frontend action registry, and the palette moved into it.** `desktop/src/lib/voiceActions.ts` exporting `VOICE_ACTIONS`, with the rail buttons and the command palette's `commandItems` (`desktop/src/App.tsx:1043-1053`) dispatching through it. **This is what makes the capability definition real rather than a parallel list**, so it lands before the guard. No behaviour change visible to a user; vitest proves each moved control still does what it did.
- [ ] **M3 — The guard.** `linkage-check` numbered rule **13** plus a `#[cfg(test)]` module holding the scanner's own tests, including planted-bad-input tests for each assertion so "no findings" is meaningful. Covers `invoke` ↔ registry, `screens` ↔ `DeckView`'s `kind` literals, param `kind` ↔ the closed set, and every registry entry classified by a row or a `no_voice` marker.
- [x] **M4 — The `[voice]` settings section and the `SecretStore`.** The section struct with all-allowlisted types, closed enums for the backend and mode choices, the panel component, one `SETTINGS_SECTIONS` row, the `PINNED_TS_FIELDS` and `normalizeDesktopSettings` extensions, and the `SecretStore` #803 M5 named — store/load/delete over the OS keychain, with a documented non-silent failure path where no Secret Service exists. Any edit to `ALLOWED_FIELD_TYPES` arrives here with its written reason.
- [ ] **M5 — Intent backends behind the seam.** The agent-CLI backend (no key, no download, the default) with fence-tolerant parsing, stdout-noise tolerance, a timeout, and PATH resolution through `login_shell`; and a keyed remote backend over the `reqwest` already in the graph. Settings choose between them. The measured latency is surfaced in the UI rather than hidden.
- [ ] **M6 — The voice surface.** The panel, typed input, the outcome-to-sentence rendering, the transcript shown on no-match, and the report with an undo window. Navigation-only commands: **at least three rows plus a deliberate no-match**, because a one-command vocabulary makes the model map everything to it and tests neither disambiguation nor the no-match path. vitest for each outcome; Playwright over the fixture bridge for the panel in both engines.
- [ ] **M7 — Microphone capture, and one activation mode.** Rust-side capture handing PCM to the `Transcriber` seam, one remote transcription backend, and press-once-to-start / press-once-to-stop. The Linux dev-package edits to `tauri-deps/flake.nix` and both `apt-get` blocks in `ci.yml` land here, as does the macOS `NSMicrophoneUsageDescription` entry in the bundle overlay. **This is the milestone to descope if the platform permission work proves worse than the survey measured** — the spine ships without it, from typed input, and saying so now is cheaper than discovering it under pressure.
- [ ] **M8 — Prove the one-file claim.** Add the next command by editing only the table and its fixtures. If that requires touching anything else, M1–M3 are not finished.
- [ ] **M9 — Phrase fixtures, in the credentialed lane.** Utterance → expected action, including availability cases in both directions, opt-in and skipped when no backend is configured, authoritative against the default backend and best-effort elsewhere. Run them locally and name them in the PR, because nothing in CI will.
- [ ] **M10 — Docs, smoke check, changelog.** `docs/develop/desktop-gui.md` gains the pipeline, the table's three consumers, the capability rule and what the guard does and does not see, plus a manual smoke check covering the microphone permission on each platform available. User-facing setup goes under `docs/` per CLAUDE.md rule 11. Changelog fragment via the `dot-ai-changelog-fragment` skill.

**Deferred, each with what it is waiting on.** None of these is dropped; a deferred milestone with a reason is the deliverable and an omitted one is a hole.

- [ ] **D1 — Local whisper transcription.** Waiting on a deliberate decision to take a C/C++ build toolchain into `cargo test-fast --workspace` and all three required build jobs, and on D3's download management. This is what removes the credential requirement from transcription, so it is the most valuable of the deferred set.
- [ ] **D2 — A local grammar-constrained intent model.** Same toolchain decision, plus D3. This is what makes an invalid action unrepresentable rather than merely unlikely, and what takes intent resolution off the 4.5-second path measured above.
- [ ] **D3 — Model download management.** Progress, retry, disk-usage visibility and removal, rendered in the #803 settings panel with the download itself Rust-side because the CSP leaves no alternative. Waiting on D1 or D2 — with no bundled engine, nothing downloads, and building this first would be a manager with nothing to manage.
- [ ] **D4 — The remaining two activation modes**, hold-to-talk and always-on with voice-activity detection. Waiting on M7 shipping and one mode being used enough to say what the other two are worth.
- [ ] **D5 — Destructive commands behind confirmation.** Approve or deny a permission prompt, close a pane, stop an agent — confirmed regardless of model confidence, because the cost of a rare misfire on "approve" is an agent doing something nobody sanctioned and a confident wrong answer is indistinguishable from a right one. Waiting on the navigation-only slice being proven in real use and on a confirmation flow; #803 recorded that the settings surface has no destructive action at all today and that `ConfirmDialog` is the intended route.
- [ ] **D6 — Modal dictation.** "Type to the tester" aims input at that agent until an exit phrase. Three things to design for, recorded now because they are the whole difficulty: intent detection keeps running during dictation to catch the exit phrase — a missed exit sends the exit phrase into the agent's prompt and a false positive truncates mid-sentence — so it needs a distinctive phrase **and** a non-voice escape; it streams into the **visible** input rather than an invisible buffer; and it **never auto-submits on exit**. Waiting on D4, since it is an interaction mode rather than a table row.
- [ ] **D7 — Discovery.** Contextual examples rendered from the table, filtered to what is callable right now, with the no-match message doubling as discovery by showing the two or three nearest things it could have done. Waiting on the table having enough rows for a list to be worth reading.
- [ ] **D8 — An audio-native single-hop backend.** Transcription and intent in one call: lowest latency and the best conversational feel, cloud-only, and it streams the microphone continuously. An option, never the default. Waiting on the seams being exercised by two backends each, so a third does not reshape them.
- [ ] **D9 — The TUI↔desktop parity matrix.** A checked-in matrix (TUI capability ↔ desktop: shipped / not-applicable / gap) with a `linkage-check` rule failing on any new TUI action that is unclassified. The issue floats this under "Explicitly rejected" as the enforceable alternative to *"whenever we add to the TUI, add it to the desktop if it makes sense"* — which is unenforceable and taxes every TUI change with a second implementation. Under the working-vs-supervisory split most entries would legitimately be not-applicable, so drift becomes measured rather than assumed. Deferred because it is a repo-wide classification exercise over the TUI's whole surface, not a part of this feature; it is named here so the rejection does not read as "nothing to do".

**What is deliberately NOT a table row, so the one-file promise is not oversold.** The table covers the closed action set, which is the case that grows per feature. Activation modes, modal dictation and the destructive-confirmation flow are plumbing: built once, not extended per feature. "After the plumbing you only touch the file" is true for *commands* and not for new *interaction modes*, which should be rare.

## Risks

- **The one-file claim is the whole design, and M2 is where it is actually won or lost.** If the palette move leaves a second dispatch path — a rail button still calling `setState` directly, say — then `invoke` references a registry that is not the app's real dispatch seam, and the guard is checking a list rather than the app. Mitigation: M8 is a milestone whose only content is proving it, and it runs before the fixtures rather than after.
- **The capability assertion is weaker than the issue's wording, and reading it as stronger is the risk.** The guard cannot discover a capability; it can only prove that everything in the registry is classified. 80 `onClick` sites in production `.tsx` are outside its sight. Mitigation: say so in the code and in the docs, at the guard's own failure message, rather than letting the next reader infer completeness.
- **The default intent backend is slow enough to feel broken.** 4.5 seconds measured, with ~3.1 s to first token, for a supervisor who just pressed a button. Mitigation: the keyed remote backend ships in the same milestone, the latency is surfaced rather than hidden, and D2 is the real fix. This risk is why the seam is not optional.
- **Microphone permission on Linux may simply not be available in-webview, and the Rust-side capture that avoids it brings a dev-package edit to three files.** Both halves were read from source rather than run, so the first real `tauri dev` window on each platform is where this gets confirmed or falsified. Mitigation: M7 is last and explicitly descopable, and the spine works from typed input without it.
- **The phrase fixtures are authoritative for one backend only**, so a contributor whose settings name a different one gets a green run that proves less than it looks like. Mitigation: it is written into the fixtures' own module docs, not only here, and other backends get a clearly-labelled smoke set.
- **Adding a `String` to the settings schema will go red, and the tempting fix is to widen the allowlist.** `ALLOWED_FIELD_TYPES` exists to force that conversation. Mitigation: the section's fields are closed enums by design, and any allowlist edit arrives with its written reason in the entry — which is the friction working, not an obstacle.
- **A row's `description` is a prompt, and nothing type-checks prose.** A wording change can silently move which utterances land where. Mitigation: the fixtures are the only defence, and they cover availability in both directions so a `screens` typo is caught too.
- **Two advisory CI jobs cover the user-visible half.** `desktop-web` and `desktop-browser` are both advisory, so a red frontend run does not block a merge. This is a standing property of the desktop app rather than something this PRD creates, and it is the reason the guard is in `linkage-check` and the substance is in the Rust tier.

## Open Questions

1. **Does the agent-CLI intent backend work from an app launched from Finder or a desktop launcher, where PATH is minimal?** `login_shell::apply_login_shell_path()` exists and is `pub`, and its only caller today is the daemon path (`src/main.rs:1459`), so the mechanism is there and the desktop crate can call it. What is unverified is whether one capture at desktop startup is enough, and what it costs — `CAPTURE_TIMEOUT` is 10 seconds and the doc comment records a real `zsh -ilc` with plugins measuring ~6 s. Answer it in M5 by measuring, not by assuming, and decide whether the capture is at startup or lazily on first use.
2. **Does a macOS `tauri dev` window need `NSMicrophoneUsageDescription` to record, or only a bundled `.app`?** The bundle overlay (`desktop/src-tauri/tauri.bundle.conf.json`) is where the packaged answer goes, and a dev-time denial would be silent. Needs a macOS host; unverifiable here.
3. **Should the guard also pin a count of interactive controls, so a new one forces a classification?** It is the only mechanical approximation to "did you forget a capability", and its cost is a red gate on routine UI edits — 80 `onClick` sites today, most of them chrome. Recommendation is no for this PR, and to revisit if a capability is in fact found to have been missed.
4. **Which model does the keyed remote intent backend default to, and is the tool-use API or a plain constrained-JSON prompt the right shape?** A tool-use call gives a genuinely constrained enum, which is closer to GBNF's guarantee than the agent-CLI backend can offer; it is also a different code path. Decide in M5 against a measured latency, and prefer the constrained one if the latency is comparable.
5. **Does the transcript belong in the settings document's reach at all — that is, is any part of an utterance persisted?** The default should be no, and stating it deliberately is cheaper than discovering that a debug log kept one. Decide in M6 and write it into the docs either way.

## Work Log

### 2026-09-18 — Created

Written from [#802](https://github.com/vfarcic/dot-agent-deck/issues/802) read as the specification rather than as a summary, against a survey of the tree at **`d7bbbd39`**. The intent, the command table, the single-tool enum, the full-list-with-`callable` trade, the situation-not-prose rule, the "none of these" affordance and the say-what-you-heard rule are all the issue's and are carried forward expanded. Six things the survey changed or settled:

**The `invoke` column cannot name a `#[tauri::command]` for the commands this PR ships, which is the one structural correction.** The issue says it does. The registry holds thirteen entries and not one of them performs a navigation — navigation is React state — so a table built that way could expose the daemon verbs and none of the commands this PR is scoped to. Resolved by reading the issue's own step 5 literally: voice dispatches where a click dispatches, a click is dispatched in the frontend, so `invoke` names an entry in one frontend action registry, and a Rust-side capability is a one-line `invoke()` inside that entry. Both registries are enumerable by text scan, which is the idiom `desktop_settings_secrets.rs` already uses on `bridge.ts` from a required gate.

**"User-facing desktop capability" has no mechanical definition the guard can discover, and the honest version is narrower than the issue's.** Adopted: a capability is an entry in the action registry, which earns the definition by being load-bearing at runtime rather than a list beside the app — which is also why M2 moves the palette's `commandItems` into it. Named cost: 80 `onClick` sites in production `.tsx` are outside its sight, so the claim is that the registry is classified, not that the app's surface is.

**The engines were staged out and the reasoning written down rather than left to look like the issue being ignored.** `desktop/src-tauri` is a workspace member, so a bundled C/C++ inference engine lands a cross-platform native toolchain in the per-task gate and in three required build jobs. The seams ship; the engines do not. An off-by-default cargo feature was considered and rejected for reproducing the #407/#436/#502 hole class.

**The no-key intent backend was measured rather than argued.** `claude -p --model claude-haiku-4-5 --output-format json` resolves a closed-set pick correctly, answers `{"action":"none"}` for "what time is it", and costs 4.49–4.68 s wall and $0.0036–$0.0126 per utterance — the price being the CLI's own ~21 k-token session context rather than the task. Output is fenced despite instructions and stdout can carry a banner, so parsing is fence- and noise-tolerant. Verdict: workable as a zero-configuration default that makes the feature try-able, slow enough that saying so is part of shipping it.

**The capture question was measured in two directions and one platform is blocked.** Playwright's WebKit has a mock audio device and **no `MediaRecorder`**; Chromium needs two launch flags and has one. Web Audio at 16 kHz works in both. In the shipping webviews, `wry` 0.55.1 grants media capture unconditionally on WKWebView, decides only clipboard-read on WebView2, and for WebKitGTK sets no `enable-media-stream` and handles no permission request at all. So webview capture would ship a macOS-only feature, and the recommendation became Rust-side capture — which the CSP was pushing toward anyway, since every network hop is Rust-side and whisper wants PCM.

**#803's credential guard will go red when this PRD adds a `String`, and its own module docs say so.** That turned the settings work from "add a section" into "add a section whose every field is a closed enum, and build the `SecretStore` #803 M5 deliberately did not build". Recorded with the guard's four checks read rather than guessed, including that `bool` is not on the fifteen-entry allowlist either, so even a plain on/off field is a deliberate edit with a written reason.

Two decisions were taken before the document and are recorded rather than re-opened: **rule 9's flag question was asked of the user and answered no**, so the issue's `## Rule 9` section is superseded and this surface ships visible by default; and **the PRD is written first and implemented against**, with milestones as the unit of delegation.

### 2026-09-18 — M1 shipped, and three corrections to this document

**M1 landed** as `desktop/src-tauri/src/voice/` — `commands.toml` with `include_str!`, the parse and its twelve named rejections, the single-tool enum schema, the `callable` computation, validation, param resolution against the live snapshot, and the outcome-to-sentence rendering, with a `StubResolver` standing in for a model. Three things it found are corrections to this document rather than to the code, and they are recorded here so M2/M3/M6 build against the corrected shape.

**The outcome set is nine, not four.** The four listed above could not express a missing param, a param that resolves to nothing, a param that resolves to more than one thing, a backend that could not answer, or a backend that named an action outside the table — and each of the first four wants a different sentence. The table above enumerates all nine. `UnknownAction` and `NoMatch` share a sentence and stay separate variants, which is written down so a later reader does not merge them.

**The success-sentence column was added and is called `report`.** The example row in this document had nothing to render on success, so without the column M6 would have invented success wording in the frontend and the "the app renders every sentence" property would have gone with it. `confirmation` was the obvious name and is the wrong one: D5 is *destructive commands behind confirmation*, so a genuine confirm-before-acting flag will live in this same file, and the two are close to opposite. Renamed while M1 was its only dependent.

**`close_agent_view` stays, and the Out of Scope entry now names both operations.** Closing an agent's *terminal pane* is lifecycle and destructive; dismissing the agent *view* is `setView({ kind: from })` and is navigation. #1105's vocabulary calls the overlay "the agent pane", so the distinction is written into the scope entry rather than left to be inferred. Without the row the `agent` screen has no callable command at all.

Also in the same commit: a CLAUDE.md rule 17 sweep over M1's own prose. Four claims were narrowed — the transcript no-logging rule was scoped to a `tracing`/`log` call this crate does not use (it logs with `eprintln!`) and was stated as an established property rather than as the rule it is; `table()`'s "a failure can only be introduced by editing `commands.toml` or the parser" omitted a `toml_edit` bump; and `outcome.rs`'s "every sentence is rendered from the table" was one quantifier too wide, since the table supplies two pieces of wording and fixed prose in that file supplies the rest. The fourth was resolved by changing the code instead of the sentence: the report template now renders in one pass, so a user-chosen agent name containing `{…}` cannot be substituted into by a later param.

### 2026-09-18 — M4: the `[voice]` section and the `SecretStore`

The section is three closed enums and nothing else: `activation` (`toggle`), `intent` (`claude` | `opencode` | `remote`) and `transcription` (`off` | `remote`), each with a folding deserializer in the `AppearanceMode` idiom and a 64-byte bound. `DesktopSettings::voice` is an `Option<VoiceSettings>` for `endpoints`' reason — `None` means *unspecified*, so a client that does not send the section cannot have the merge write defaults over a choice the user made.

**No model identifier and no stored-key boolean**, both deliberately. Open Question 4 is M5's to answer against a measurement, so a `model` field now would be this section inventing that answer. The boolean the credential rule explicitly allows is absent because the panel asks the `SecretStore` itself — an answer that cannot go stale against the keychain — which also keeps `bool` off `ALLOWED_FIELD_TYPES`.

**The guard went red exactly where its own module docs said it would**, at all four new fields, and the resolution was the first of the two honest ones it names: the values that could have been a `String` are closed enums, and the value that genuinely is a credential went behind the seam. `ALLOWED_FIELD_TYPES` gained four entries, each with its written reason.

**The `SecretStore` is built, and two of its properties are decisions rather than details.** There is **no IPC command that reads a credential back** — the webview may ask whether one is stored, replace one and forget one, and `load` is Rust-side only, where M5's and M7's backends make their network call because the CSP forces every hop there anyway. And **a failed store is an `Err`, never a status saying "nothing stored"**: the three situations #803 asked to be handled — no credential store on this machine, a locked one, anything else the platform reports — each carry a sentence naming what did not happen, because the outcome to design against is a user who thinks their key is saved and finds voice broken tomorrow.

**`keyring` 4.2.0 on default features, and the cost is that it adds NO system package on any of the three platforms.** `default = ["v1"]` selects one store per target, each target-gated in `keyring`'s own manifest: the Security framework on macOS, `windows-sys` on Windows, and `zbus-secret-service-keyring-store` on Linux — zbus speaks D-Bus in Rust rather than binding `libdbus`, so `tauri-deps/flake.nix` and both `apt-get` blocks in `ci.yml` are untouched. That is the one shape of cost this PRD's dependency accounting said to avoid, and it was avoidable here; M7's `alsa-lib` still is not. The two alternatives were each worse in a stated way: `dbus-secret-service` wants `libdbus-1-dev`, and `linux-keyutils` wants no package and keeps its keys in the kernel, where they do not survive a reboot — which a user experiences as the app forgetting their key overnight. It costs 59 lockfile entries and, measured on 16 cores, 13.9 s wall / 66 s CPU to build all of them from a clean slate; warm it is a no-op.

**One correction to this document's own accounting**, which said `keyring` was needed "and the Linux Secret Service failure path needs designing rather than assuming". It did, and the design is above — but the sentence beside it implied the dependency would cost a dev package the way the audio crate will. It does not, and saying so matters because the two were being weighed together.
