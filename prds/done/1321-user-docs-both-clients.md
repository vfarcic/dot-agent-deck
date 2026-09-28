# PRD #1321: User docs for both clients — common, TUI and desktop sections, with screenshots

**Status**: Complete (2026-09-28)
**Priority**: Medium
**Created**: 2026-09-27
**Issue**: [#1321](https://github.com/vfarcic/dot-agent-deck/issues/1321)
**Absorbs**: [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) (document installing the desktop GUI) — the PR that completes this PRD closes #765
**Depends on**: [#1045](https://github.com/vfarcic/dot-agent-deck/issues/1045) (one glossary), [#746](https://github.com/vfarcic/dot-agent-deck/issues/746) (logo), [#757](https://github.com/vfarcic/dot-agent-deck/issues/757) (signed desktop artifacts) — all three closed as of 2026-09-27
**Uses**: [#1322](https://github.com/vfarcic/dot-agent-deck/issues/1322) (`cargo docs-screenshots`, closed as of 2026-09-27) — not a blocker
**Interacts with**: [#1308](https://github.com/vfarcic/dot-agent-deck/issues/1308), [#1309](https://github.com/vfarcic/dot-agent-deck/issues/1309), [#1310](https://github.com/vfarcic/dot-agent-deck/issues/1310), [#1311](https://github.com/vfarcic/dot-agent-deck/issues/1311), [#1312](https://github.com/vfarcic/dot-agent-deck/issues/1312) (the desktop graduations, open as of 2026-09-27), [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (the desktop GUI PRD whose user-doc item was left unchecked)

## Problem Statement

The published docs site (`docs/`, served at agent-deck.devopstoolkit.ai) describes the TUI only. The desktop app has no user-facing documentation: PRD #176 closed with M5.3's "user doc once past spike quality" unchecked, and #765 deferred the install page until the app left unsigned alpha. The desktop app appears only in passing in five pages (configuration, troubleshooting, remote environments, remote requirements, idle workers). Separately, the docs as a whole are mostly text: 5 of 15 pages carry any image.

With the first signed desktop release shipped by #757 (v0.42.0, a Developer ID signed and notarized macOS `.dmg`), the desktop app becomes something we want people to install on purpose, and the docs need to present both clients.

## Solution Overview

Restructure the user docs around the fact that the TUI and the desktop app are two clients of the same daemon, document the desktop app, and add screenshots throughout. Six parts:

### 1. Analyse the desktop features from the code first

Before writing any page, build an inventory of the desktop app's user-visible features **from the code** (`desktop/src/`, `desktop/src-tauri/`), not from the developer docs. Reading `docs/develop/desktop-gui.md` and the desktop PRDs is fine as context, but they are not the source of truth. For each feature, record where it lives in the code, whether the TUI has the same feature, and whether it is behind the `experimental` flag. The inventory is checked into this PRD (see [Desktop feature inventory](#desktop-feature-inventory)); it drives the page structure and the screenshot list. It is taken when the work starts (milestone 1), not when this document is written, because the feature set is still moving.

### 2. Split the docs into three groups

- **Common**: features both clients have. They are both daemon clients, so this should be most of it.
- **TUI-specific**.
- **Desktop-specific**.

`site/sidebars.js` is organised to match.

### 3. Home page features both clients

The docs site home page (`site/src/pages/index.js`) presents both the TUI and the desktop app.

### 4. Install page

The install page **absorbs #765**: which desktop artifact to download for which platform, per-platform install and first launch, how the app finds or starts a daemon, and how the GUI relates to the TUI (same daemon, second client). It describes the signed install that #757 delivered. The PR that completes this PRD closes #765.

### 5. Screenshots

- Screenshots are added across the docs in general, not only to the desktop part.
- A feature common to both clients that is illustrated with a screenshot gets one from **each** client, shown as tabs (TUI | Desktop) so the page does not double in length.
- Screenshots go stale, and that is accepted; the work does not wait for a final version.
- **Screenshots come from `cargo docs-screenshots`** (issue #1322, [`docs/develop/docs-screenshots.md`](../../docs/develop/docs-screenshots.md)), not from hand captures. Each new screenshot is a new or extended scenario in that tool's registry (`xtask/screenshots/src/scenarios.rs`, with its TUI capture in `tests/e2e_docs_screenshots.rs` and its desktop capture in `desktop/screenshots/desktop.shot.ts`), so it can be regenerated when the UI changes. A feature both clients have uses the **same** scenario name on both, depicting the same state, which is what makes the TUI | Desktop tabs possible (`dashboard` is the worked example). A screen the tool genuinely cannot reach is the exception, and is recorded as such where it is used.

### 6. A skill that makes agents consider screenshots whenever they change docs

Add a **project-local skill** under `.claude/skills/`, **without the `dot-ai-` prefix** (CLAUDE.md rule 13: `dot-ai-*` skills are a synced mirror and a project edit there is overwritten). It is a skill rather than a `CLAUDE.md` rule, since most work does not touch docs and `CLAUDE.md` is loaded in full every session. Its description should trigger on any edit to user-facing docs under `docs/`, in any task, not only this PRD. The instruction: evaluate whether the change warrants a new or updated screenshot, and if it does, either produce it (through `cargo docs-screenshots`, or the TUI via the `run-dot-agent-deck` skill where no scenario fits) or tell the user exactly which screenshot to capture. If agents turn out not to invoke it reliably, a hook scoped to edits under `docs/**` is the fallback; that hook is not built up front.

## Desktop feature inventory

**Taken 2026-09-27 (M1), from the code at `a52c7f50`.** Paths are relative to the repository root; `src/…` is the root crate (the TUI and the daemon), `desktop/…` the desktop app. Line numbers are approximate pointers into files that move. The last column decides what gets documented (see Constraints), and the docs say which desktop features are behind `experimental` according to it.

**How the desktop reads the flag.** The desktop reads `experimental` from its own process environment only, once at startup: `DOT_AGENT_DECK_EXPERIMENTAL`, or the file named by `DOT_AGENT_DECK_FEATURES_CONFIG` (`features::init_from_process_env`, called from `desktop/src-tauri/src/lib.rs` ~2317). It does not look for a `.dot-agent-deck.toml` at all. Five wrappers in `src/features.rs` (~133–169) — `show_desktop_deck`, `show_desktop_projects`, `show_desktop_prompts`, `show_desktop_orchestrations`, `show_desktop_agent_profiles` — reach the webview as the `DesktopFeatures` DTO (`desktop/src-tauri/src/dto.rs` ~1292, the `desktop_features` command), and every gate reads `desktopFeaturesOf(runtime)` (`desktop/src/types.ts` ~1356), whose default is all `false`. With the flag off the navigation rail (`desktop/src/components/NavigationRail.tsx`) shows **Dashboard** and **Settings** only, and a view that asks for the deck screen is shown as the Dashboard instead (`overviewInsteadOfDeck`, `desktop/src/App.tsx` ~156). One more flag reaches the desktop and is not the app's: the New agent dialog's `schedule: issues` chip follows the **daemon's** flag (see the New agent row).

**Graduated (always shown).** These are what M3 documents.

| Feature | Where it lives in the code | TUI has it? | Behind `experimental`? |
| --- | --- | --- | --- |
| **Dashboard** — every agent as a row, with a status word and the columns below; clicking a row opens its agent pane; Refresh | `desktop/src/components/AgentOverview.tsx` (`AgentOverview`, `OverviewRow`); live data from `desktop/src-tauri/src/agent_view.rs` | **Yes, as cards.** The Dashboard tab and `render_session_card` (`src/ui.rs` ~21144). Different fields: a TUI card shows `Dir:` (the last path component), `Prmt:`, recent tool lines and `Last:` / `Tools:`; a desktop row has Status, Agent, Last activity, Uptime, CLI, Active tool, Tools, Working directory and Last prompt columns. The TUI has no uptime field. | No |
| **Several daemons on one Dashboard** — one section per daemon, each with its own state and a New agent button; fleet counters AGENTS / RUNNING / WAITING / FAILED / GROUPS and DAEMONS up/total | `AgentOverview.tsx` (`DeckGroup`, `OverviewInstrument`, `decksUpTitle`) | **No.** The TUI attaches to one daemon at a time (`Endpoint`, `src/daemon_client.rs:68`). | No |
| **Daemon selector** — "All daemons", "This machine", one entry per remote daemon added in Settings | `desktop/src/components/DeckSelector.tsx`; choices from `deckChoices` in `desktop/src/lib/endpoints.ts` ~491 | **No in-app selector.** `dot-agent-deck connect [name]` picks one remote before the TUI starts ("Select a remote:", `src/connect.rs` ~291). | No |
| **Agent statuses**, including Blocked and why | `DAEMON_STATUS` in `desktop/src/lib/bridge.ts` ~1782; `desktop/src/lib/blockedReason.ts` | **Yes, with more words.** `status_style` in `src/ui.rs` ~21505: Thinking, Working, Compacting, Needs Input, Idle, Error, Blocked. The desktop folds them into running (Thinking/Working/Compacting), waiting (Needs Input *and* Idle), failed, blocked. Glossary "Known exceptions" defers the desktop's words to #1043. | No |
| **Grouping by orchestration and mode tab** — "Standalone agents" first, then one card per orchestration (kicker ORCHESTRATION) and per mode tab (MODE TAB); an ORCHESTRATOR badge on the start role | `groupAgents` (~184) and `OverviewGroupCard` (~1692) in `AgentOverview.tsx` | **Partly.** The TUI groups by tab: one tab per mode or orchestration, labelled `name [active]` / `name [done]` (`src/ui.rs` ~3222). No ORCHESTRATOR badge was found in the TUI's card rendering; role cards carry the role name. | No |
| **Columns chooser** — pick the Dashboard's columns; remembered per browser profile, not in the settings file | `OverviewColumnPicker` in `AgentOverview.tsx` ~1545 (localStorage) | **No.** The card grid is sized from the terminal (`choose_grid_layout`, `src/ui.rs` ~247). | No |
| **Close an agent** — "Close {name} agent" on a row, confirmed with "Close agent" | `AgentOverview.tsx` (~1010), `ConfirmDialog.tsx`; sends `stop_agent` | **Yes.** `Ctrl+W` or [Close] in command mode, "Close selected agent?" (`src/ui.rs` ~7058; `src/keybindings.rs:147`). | No |
| **Close an orchestration** — the group card's Close, confirmed with "Close all N roles" | `AgentOverview.tsx` (~1022, ~1739); sends `stop_orchestration` | **Yes, as closing its tab.** "Close this tab and all its agents?" (`src/ui.rs` ~7050). | No |
| **Agent pane** — the agent's live terminal in a full-window pane over the Dashboard; type into it; Escape or "Back to dashboard" closes it | `OverviewAgentPane` / `AgentPaneFrame` (`desktop/src/App.tsx` ~880–1010), `desktop/src/components/AgentTile.tsx`, `TerminalViewport.tsx` (xterm.js), `desktop/src-tauri/src/terminal.rs` | **Yes.** Panes and PaneInput mode (`UiMode::PaneInput`, `src/pane_input.rs`, `Ctrl+D`). | No. Note what a user sees: besides Terminal the pane has Diff, Checks, Delegations and Artifacts tabs, and against a live daemon each shows a "… not exposed by the daemon" placeholder (`AgentTile.tsx` ~518–551). The pane has no rename control (`App.tsx` ~930). |
| **New agent** — "New agent" (Dashboard, and per daemon section) or `Ctrl+N` / `⌘N`; choose a daemon, browse its directories (through the daemon, with a filter and hidden directories), pick a Mode chip — No mode, `Orch: <name>` for each orchestration of that directory's project, `schedule`, `schedule: issues`, `dispatcher` — then Name and Command; "Create agent" or "Activate orchestration" | `desktop/src/components/NewAgentDialog.tsx`, `desktop/src/lib/newAgent.ts`; `desktop_list_directories`, `desktop_new_agent_options`, `desktop_new_agent_orchestrations` in `desktop/src-tauri/src/lib.rs` | **Yes.** `Ctrl+N`, the directory picker (`render_dir_picker`, `src/ui.rs` ~19926), the New Agent form with Mode chips (`src/ui.rs` ~20561–20695, `Orch:` ~1511). Differences: the TUI reads its own filesystem and skips hidden and symlinked directories (`docs/develop/tui-desktop-parity.md`); the TUI also offers the project's `[[modes]]` as Mode chips, and the desktop offers none (its `ModeId` has no workspace-mode variant, `NewAgentDialog.tsx` ~137). | No for the dialog. The desktop shows the `schedule: issues` chip only when the **daemon's** flag is on (`options.experimental` in `authoringModes`, `desktop/src/lib/newAgent.ts` ~176). The TUI gates its own chip on its own flag (`show_issue_dispatch_authoring`, `src/features.rs:98`), which it reads from the same `.dot-agent-deck.toml` as the daemon. |
| **Daemon connection states** — "Daemon disconnected", "Incompatible daemon", "Daemon not configured", "Waiting for this daemon", "No agents are running yet"; Reconnect; "Connect anyway" to a daemon whose build stamps differ | `DaemonBody` in `AgentOverview.tsx` ~1371; handshake in `desktop/src-tauri/src/daemon_bridge.rs` (`build_stamp_mismatch_only`) | **Partly.** A build mismatch is settled before the TUI opens: "Daemon version mismatch" with `[S] restart daemon` or keep the current one (`src/build_version_handshake.rs` ~692); with no agents running it restarts silently. | No |
| **Settings → Daemons** — add and remove remote daemons (Host, User, Port, Key file, Jump host, Daemon socket), "Test connection" with named results | `desktop/src/components/EndpointsPanel.tsx`; ssh tunnels in `desktop/src-tauri/src/endpoint_tunnels.rs`; `desktop/src-tauri/src/endpoint_test.rs` | **No screen; the CLI.** `dot-agent-deck remote add\|list\|remove\|doctor\|upgrade` and `connect` (`src/main.rs` ~498–556). The desktop reaches a remote daemon through an ssh tunnel and does not start one there (`daemon_bridge.rs` ~1446). | No |
| **Settings → Appearance** — System / Light / Dark (agent terminals stay dark) | `desktop/src/components/AppearancePanel.tsx`, `desktop/src-tauri/src/appearance.rs` | **No.** The TUI uses the terminal's palette (`src/palette.rs`). | No |
| **Zoom** — Settings → Zoom (75%–300%) and `Ctrl`/`⌘` with `=`, `-`, `0` on every screen | `desktop/src/components/ZoomPanel.tsx`, `desktop/src/hooks/useZoom.ts`, `desktop/src/lib/zoom.ts`; `desktop_set_zoom` | **No.** The TUI's "zoom" (`Ctrl+Z`) is a different feature: the focused pane takes the whole frame. | No |
| **Voice control** — the Voice button (off at launch), "What you can say", Undo; Settings → Voice (speech and commands endpoints, models, keys stored in the OS keychain) | `desktop/src/components/VoiceControlPanel.tsx`, `VoicePanel.tsx`, `desktop/src/lib/voiceActions.ts`; `desktop/src-tauri/src/voice/*`; keys in `desktop/src-tauri/src/secrets.rs` | **No** (`docs/develop/tui-desktop-parity.md`). | No. Speech defaults to a local container; commands default to a hosted endpoint that needs an API key (`desktop/src/lib/bridge.ts` ~623). |
| **Settings file** — the Settings footer names `desktop.toml` in the config directory | `desktop/src-tauri/src/settings.rs` (`settings_path`, `DOT_AGENT_DECK_DESKTOP_CONFIG`) | The TUI has its own files (`config.toml`, `keybindings.toml`); none is shared with the desktop. | No |
| **Cleanup warning** — "N roles may still be running on {daemon}: their stops could not be confirmed" in the New agent form and as a toast | `desktop/src/components/CleanupWarning.tsx`, `Toast` in `App.tsx` ~1920 | **No equivalent found.** The TUI's Close dialog warns about something else: uncommitted work in a worktree is kept (`src/ui.rs` ~7317). | No |

**Experimental (hidden by default).** Out of scope for user docs (see Constraints) until their graduation issue closes; the docs say they exist and are behind `experimental`.

| Feature | Where it lives in the code | TUI has it? | Behind `experimental`? |
| --- | --- | --- | --- |
| **Deck screen** — the multi-pane workspace behind the rail's "Daemons" entry and the Dashboard's "Open daemons": agent tiles, the run graph, "Live delegations", the Events drawer, the command palette (`⌘K`), the keyboard-shortcut sheet | `ControlDeck` / `DeckSurface` in `desktop/src/App.tsx` ~1013; `desktop/src/components/HandoffRail.tsx` | Panes: yes. A delegations or events view: **no** such screen was found in the TUI. Shortcut help: yes (`?`, `src/ui.rs` ~19700). | **Yes** — `show_desktop_deck`, #1308 |
| **Start / Stop / Replace daemon** | The deck's connection banner and Stop button (`App.tsx` ~1446–1530); `desktop_bootstrap { startIfMissing: true }`, `StopDaemon`, `RestartDaemon` in `desktop/src-tauri/src/lib.rs` | The TUI starts its local daemon itself on launch; Stop is in the quit dialog (`src/ui.rs` ~19133); `dot-agent-deck daemon stop\|restart`. | **Yes, by being on the deck screen** (#1308). So with the flag off **the desktop starts no daemon**: `connect()` bootstraps with `startIfMissing: false` (`desktop/src/lib/bridge.ts` ~3683), and `start_daemon` is dispatched only from the deck (`App.tsx` ~1522). |
| **Rename an agent** | The pencil in `AgentTile.tsx` ~317, passed only on deck tiles in live mode | **Yes.** `r` or [Rename] (`src/keybindings.rs:280`). | **Yes, by being on the deck screen** (#1308) |
| **Output reader** — "Reader", "Copy all", live tail | `desktop/src/components/OutputReader.tsx`, reachable only from deck tiles | No equivalent found (PageUp/PageDown scrollback in command mode is the nearest). | **Yes, by being on the deck screen** (#1308) |
| **Projects** panel | `ProjectsPanel`, `desktop/src/components/ConfigurationPanels.tsx` ~49 | No dedicated panel found. | **Yes** — `show_desktop_projects`, #1309 |
| **Prompts** library | `PromptLibraryPanel`, `ConfigurationPanels.tsx` ~154 | No. | **Yes** — `show_desktop_prompts`, #1310 |
| **Orchestrations** editor ("Edit orchestration order") | `OrchestrationPanel`, `ConfigurationPanels.tsx` ~426 | No editor; orchestrations come from `.dot-agent-deck.toml`, which `g` can generate. | **Yes** — `show_desktop_orchestrations`, #1311 (whose text still says `show_desktop_workflows`) |
| **Agent Profiles** | `ProfilesPanel`, `ConfigurationPanels.tsx` ~214 | No (glossary: desktop-only). | **Yes** — `show_desktop_agent_profiles`, #1312 |

Voice's `open_deck` command follows `show_desktop_deck` too: it is left out of "What you can say" and refused while the deck is hidden (`VoiceControlPanel.tsx` ~572, `desktop/src-tauri/src/voice/schema.rs` ~213).

**TUI features the desktop does not have**, for the TUI-specific group: workspace mode tabs with side panes (`[[modes]]`), the Schedules manager (`s`), Filter agents (`/`), keybinding customisation (`keybindings.toml`), automatic session save and restore, the quit dialog's Detach / Stop, the terminal bell on status changes, `1`–`9` to jump to a card, and `y` / `n` on a permission prompt. The command-entry lock (`Ctrl+E`) is TUI-only and itself experimental (`show_command_entry_lock`).

### What a TUI user would see differently — open questions

The PRD's rule is the glossary's (#1045): the TUI's words, and where the TUI is itself inconsistent the user decides. These are listed, not decided.

1. **Status words.** The TUI shows Thinking / Working / Compacting / Needs Input / Idle / Error / Blocked; the desktop shows running / waiting / failed / blocked, with Idle and Needs Input both "waiting" (`DAEMON_STATUS`). The glossary leaves the desktop's words to #1043. Question: does the common statuses section use the TUI's words and document the desktop's mapping, or wait for #1043?
2. **Closing an orchestration.** The TUI's dialog says "Close this tab and all its agents?" (`src/ui.rs` ~7050), never "orchestration", while the glossary's TUI column speaks of "orchestration tab" and the desktop says "Close {name} orchestration" / "Close all N roles". Question: which wording do the common docs use?
3. **"New Agent" or "New agent", and "create".** The TUI's button and form title say "New Agent" but its help line says "Create new agent" (`src/ui.rs` ~19722); the desktop says "New agent" and "Create agent". Question: which casing and verb do the docs use?
4. **"Zoom" means two things.** TUI `Ctrl+Z` makes the focused pane fill the frame; desktop Zoom scales the window. The glossary's Homonyms section does not list it. Question: add it to the glossary, or name one of them differently in the docs?
5. **Last activity and uptime.** TUI `Last:` versus desktop "Last activity"; the desktop's "Uptime" has no TUI counterpart. Question: the docs' label for the shared concept.
6. **The local daemon's name inside the desktop.** The daemon selector says "This machine" (`desktop/src/lib/endpoints.ts` ~494) while other desktop text says "Local daemon" (`desktop/src/lib/displayText.ts` ~481), which is the glossary's word. A desktop-internal inconsistency. Question: which word the docs use, and whether it is a bug against the glossary.
7. **Two surfaces called "Daemons".** Settings → Daemons (graduated) and the rail's "Daemons" entry for the deck screen (experimental). The glossary names both. Not a problem today, since only one is documented, but it becomes one when #1308 graduates.
8. **The ORCHESTRATOR badge.** The desktop marks the start role; no TUI marker was found. Question: do the common orchestration docs mention the badge only under the desktop tab?

### Findings that change what the pages must say

- **With the flag off, the desktop app never starts a daemon.** It only connects (`startIfMissing: false`); Start daemon and Replace daemon are on the experimental deck screen, and the first-run hint reads "Start one, then reconnect." The install page (M4) must therefore say how a user gets a daemon running for the desktop without the flag — the candidates are the TUI starting one and `dot-agent-deck daemon serve`, neither tried for this inventory, and the daemon's 30-second idle shutdown (`DEFAULT_IDLE_SHUTDOWN_SECS`, `src/daemon.rs`) has to be checked against the second — and it must be checked against the bundled daemon the `.dmg` carries (`externalBin` in `desktop/src-tauri/tauri.bundle.conf.json`), which the flag-off app has no control to start. This is a question for the user before M4 is written, not something this PRD can change (code is out of scope).
- **The desktop cannot add a remote daemon's binary or start it.** A remote added in Settings → Daemons must already be running; the TUI's `remote add` / `connect` installs and starts it. The remote pages need a desktop note.
- **The agent pane's four non-terminal tabs are placeholders** against a live daemon. The desktop pages should say so rather than describe them.

## Docs plan

Derived from the inventory in M1 (2026-09-27). A proposal for M2–M8, not a decision; the open questions above are the user's.

### 1. Existing pages, their group, and the new structure

Every page stays at its current path. In this site a page's URL comes from its file path (`routeBasePath: 'docs'`, and no page sets `slug` or `id` in its frontmatter), not from the sidebar, so regrouping `site/sidebars.js` moves no URL. That matters because nothing would catch a moved URL: `@docusaurus/plugin-client-redirects` is not a dependency (`site/package.json` lists only `@docusaurus/core`, `@docusaurus/preset-classic`, `@mdx-js/react`, `clsx`, `prism-react-renderer`, `react`, `react-dom`) and the hosting configs (`site/nginx-default.conf`, `site/netlify.toml`) define no redirects, while `onBrokenLinks: 'throw'` covers internal links only. `site/docusaurus.config.js`'s footer and `site/src/data/landing-content.js` (~620) also link to `/docs/getting-started`, `/docs/installation`, `/docs/orchestration`, `/docs/dispatcher-mode`, `/docs/configuration`, `/docs/keyboard-shortcuts` and `/docs/remote-environments` by URL. If M2 decides to move a page anyway (say, TUI pages into `docs/tui/`), it needs that plugin added, or accepts that outside links break.

| Page | Group | What changes |
| --- | --- | --- |
| `getting-started.md` | Common, with TUI \| Desktop tabs | Today a TUI quick start. "Launching" and "Basic workflow" get a Desktop tab (first launch, the Dashboard, New agent). |
| `installation.md` | Common | Mentions the desktop app nowhere today. Gains a desktop section (M4, absorbs #765): `dot-agent-deck-desktop-alpha-macos-arm64.dmg` and `dot-agent-deck-desktop-alpha-linux-amd64.deb`, no Windows bundle, first launch, and how the app gets a daemon (see [Findings](#findings-that-change-what-the-pages-must-say)). |
| `session-management.md` | Common, with a TUI-only section | "Session Statuses" is common (with the desktop's status words, open question 1). "Resuming Sessions" (the quit dialog's Detach / Stop, automatic restore) is TUI-only. |
| `keyboard-shortcuts.md` | TUI-specific | Every key on it is the TUI's. The desktop's few keys (`Ctrl`/`⌘`+`N`, the zoom keys, Escape) go on a desktop page. |
| `orchestration.md` | Common, with TUI-only sections | The configuration, roles, delegation, role library and reference are the daemon's and common. "Starting an orchestration tab", its navigation, `Ctrl+Z` and the command-entry lock are TUI. The desktop tab covers activating from New agent (`Orch:` chip, "Activate orchestration"), the group card and ORCHESTRATOR badge, and "Close all N roles". |
| `idle-workers-and-notifications.md` | Common | Owned by the daemon. |
| `workspace-modes.md` | TUI-specific | Mode tabs with side panes are the TUI's; the desktop cannot start a mode (no chip) and shows a mode tab's agents only as a MODE TAB group. The page should say that. |
| `dispatcher-mode.md` | Common | Both New agent flows offer the `dispatcher` chip, and `dispatch` is the CLI. Its "Starting a dispatcher pane" section needs a Desktop tab. |
| `scheduled-tasks.md` | Common, with a TUI-only section | Schedules run in the daemon and `schedule` is the CLI. "The Schedules dialog" is TUI-only; the desktop authors a schedule through the New agent `schedule` chip and has no manager. |
| `configuration.md` | Common | The daemon's and the project's config. Gains a pointer to the desktop's own settings file. |
| `remote-environments.md` | Common, split | Its flows are the TUI's (`remote add`, `connect`, stop vs detach, the upgrade nudge). The Desktop tab covers Settings → Daemons, and says the remote daemon must already be running. |
| `remote-requirements.md` | Common | Host requirements. |
| `remote-recipes.md` | Common | Host bootstrap; `remote doctor` is the CLI. |
| `troubleshooting.md` | Common, sections labelled by client | Several sections already speak to both clients (pane size, focus, hooks). |
| `license.md` | Common | — |

**New pages, Desktop-specific**, under `docs/desktop/` (new URLs `/docs/desktop/…`, so no redirect question), one per graduated feature group in the inventory:

- `desktop/index.md` — **Desktop app**: a second client of the same daemon; what it shows by default; the list of experimental surfaces (the inventory's second table) and that the desktop reads the flag from its environment only, once at startup.
- `desktop/dashboard.md` — **Dashboard**: rows and columns, the Columns chooser, statuses, grouping, several daemons, closing an agent or an orchestration, connection states, the agent pane.
- `desktop/new-agent.md` — **New agent**: the daemon list, the directory browser, the Mode chips, activating an orchestration.
- `desktop/daemons.md` — **Daemons**: the daemon selector, Settings → Daemons, Test connection and its results, what a remote daemon must already have.
- `desktop/settings.md` — **Settings**: Appearance, Zoom, the settings file, the desktop's keyboard shortcuts.
- `desktop/voice.md` — **Voice control**: the Voice button, what you can say, Settings → Voice, where keys are stored and what is sent where.

Proposed `site/sidebars.js` (category labels are placeholders for M2):

```js
docs: [
  'getting-started',
  'installation',
  {
    type: 'category',
    label: 'Both clients',
    items: [
      'session-management',
      { type: 'category', label: 'Orchestration', link: { type: 'doc', id: 'orchestration' }, items: ['idle-workers-and-notifications'] },
      'dispatcher-mode',
      'scheduled-tasks',
      'configuration',
      { type: 'category', label: 'Remote Environments', link: { type: 'doc', id: 'remote-environments' }, items: ['remote-requirements', 'remote-recipes'] },
    ],
  },
  { type: 'category', label: 'Terminal UI', items: ['keyboard-shortcuts', 'workspace-modes'] },
  {
    type: 'category',
    label: 'Desktop app',
    link: { type: 'doc', id: 'desktop/index' },
    items: ['desktop/dashboard', 'desktop/new-agent', 'desktop/daemons', 'desktop/settings', 'desktop/voice'],
  },
  'troubleshooting',
  'license',
],
```

### 2. Tabs in the existing `.md` pages

By configuration they should work, and this has not been built yet. `site/docusaurus.config.js` sets no `markdown` key, so `markdown.format` is Docusaurus 3's default, `mdx`, under which `.md` files are compiled as MDX too; the lockfile resolves `@docusaurus/core` 3.10.2. `@theme/Tabs` and `@theme/TabItem` come with `@docusaurus/theme-classic`, which `@docusaurus/preset-classic` pulls in (the lockfile lists it at 3.10.2). So a page can `import Tabs from '@theme/Tabs'; import TabItem from '@theme/TabItem';` below its frontmatter and use `<Tabs groupId="client">` with `TabItem value="tui"` / `value="desktop"`; a shared `groupId` keeps a reader's choice across every tab set on the site. Markdown images inside a `TabItem` need a blank line on each side. M2 proves it on the first page it converts, with `npm run build` in `site/`, before relying on it.

### 3. Screenshots

Existing scenarios in `xtask/screenshots/src/scenarios.rs`: `dashboard` and `dashboard-empty`, both clients. Their four PNGs are committed under `site/static/img/` and **no page uses them yet**. The hand captures already on pages (`getting-started-launching.jpg`, `home-hero-dashboard.jpg`, `session-management-card.jpg`, `modes.png`, `orchestration-*.png`, `detach.webp`) are not from the tool; they stay until a scenario covers the same screen.

Cost, from `docs/develop/docs-screenshots.md`: a desktop scenario is a `desktopScenario(…)` call of a few lines in `desktop/screenshots/desktop.shot.ts`, plus its own fixture state in `desktop/src/data/fixture.ts` when it must mirror a TUI scene. A TUI scenario is an `#[ignore]`d test in `tests/e2e_docs_screenshots.rs` that stages the scene with stand-in commands and synthetic hook events; `docs_screenshot_dashboard` runs from ~426 to ~626 of that file, most of it the scene and its capture-second timing. A scene with no agent ages to line up is much cheaper.

| Page | Scenario | Client | Status | Notes |
| --- | --- | --- | --- | --- |
| `getting-started.md` (Launching), `session-management.md` (statuses) | `dashboard` | both, as tabs | existing | The four-agent scene in mixed states. |
| `getting-started.md`, `installation.md` (first launch) | `dashboard-empty` | both, as tabs | existing | What a fresh install shows. |
| `getting-started.md` (Basic workflow), `desktop/new-agent.md` | `new-agent` | both, as tabs | new | The New Agent form with a directory chosen. TUI: `open_pane` already drives `Ctrl+N`, the picker and the form, so the capture stops before submitting. Desktop: the fixture already has a directory tree and project orchestrations (`fixtureDirectoryTree`, `fixtureProjectOrchestrations`). |
| `orchestration.md` | `orchestration` | both, as tabs | new | An activated orchestration: the TUI's orchestration tab, the desktop's group card with the ORCHESTRATOR badge. The most expensive: the TUI scene needs an `[[orchestrations]]` config in the sandbox project and stand-ins as its roles. Would replace `orchestration-start.png`. |
| `desktop/dashboard.md` | `dashboard-fleet` | desktop | new | Several daemons on one Dashboard. The TUI cannot show this. The fixture has a `fleet` state, but it carries demo paths, so a docs fleet state is needed. |
| `desktop/dashboard.md` | `agent-pane` | desktop | new | The full-window agent pane. The TUI's `dashboard` image already shows a focused pane. What the fixture draws in the terminal is still to check. |
| `desktop/daemons.md`, `remote-environments.md` (Desktop tab) | `settings-daemons` | desktop | new | Settings → Daemons with one remote. A successful Test connection **result** is not reachable: the fixture never tests, and answers "Browser preview — it has no way to reach a daemon, so nothing was tested." (`testEndpoint`, `desktop/src/lib/bridge.ts` ~2511). The image shows the form, and that exception is recorded where it is used. |
| `desktop/voice.md` | `settings-voice` | desktop | new | Settings → Voice. |
| `scheduled-tasks.md` | `schedules` | TUI | new | The Schedules manager with one schedule. The desktop has no manager. |
| `keyboard-shortcuts.md` | `help` | TUI | new | The `?` overlay. The cheapest TUI scene. |

That is two existing scenarios (both clients) and eight new ones: two with both clients, four desktop-only, two TUI-only. A new scenario that ever runs a real agent must redact first, per the tool's page; none of these do.

### 4. How M8 (publish the docs) happens

Facts from `.github/workflows/docs-publish.yml`, `.github/workflows/release.yml` and `.claude/skills/publish-docs/SKILL.md`:

- `docs-publish.yml` has two triggers: `workflow_call` and `workflow_dispatch`.
- **Every release run publishes the docs.** `release.yml` runs on a pushed `v*` tag (which `tag-release.yml` pushes when `/tag-release` dispatches it) and on a manual `workflow_dispatch`. Its `docs` job (`needs: [prepare, finalize]`, no `if:`) calls `docs-publish.yml` with `image_tag: v<version>`, `chart_version: <version>` and `push_latest: true`, so it runs whenever `prepare` and `finalize` succeed.
- What a publish does: builds `site/Dockerfile` and pushes `ghcr.io/vfarcic/dot-agent-deck-docs:<tag>` (plus `:latest` on the release path), sets `image.tag` in `site/helm/values.yaml` (and `version` / `appVersion` in `site/helm/Chart.yaml` on the release path), and pushes that commit to `main` with `RELEASE_TOKEN`. Argo CD picks it up, per the skill. Two further jobs build the site with `npm run build` and deploy it to Netlify production; per the skill, that copy is not what `agent-deck.devopstoolkit.ai` serves until DNS is switched (not checked here).
- **What gets published is `main`, not the tag.** The `publish` job checks out `ref: main`, and the Netlify jobs check out the commit it resolved. So a docs PR merged to `main` before a release goes out with that release, and one merged after goes out with the next.
- **Between releases**, `/publish-docs` runs `gh workflow run docs-publish.yml --ref main`: the tag is `main-<short sha>`, `:latest` is not moved, the commit message is `chore: publish docs image main-<sha> [skip ci]`, and the next release re-pins the chart to `v<version>`.
- So M8 needs no separate step if the docs land on `main` before the release this PRD targets; `/publish-docs` is only for publishing sooner. `site/helm/values.yaml` currently pins `v0.43.0`.
## Scope

### In scope

- The desktop feature inventory, from the code, checked into this PRD.
- Restructuring `docs/` into common / TUI / desktop groups and reorganising `site/sidebars.js` to match.
- Desktop pages for every graduated desktop feature.
- The install page, absorbing #765.
- The home page presenting both clients.
- Screenshots across the docs, generated through `cargo docs-screenshots`, with TUI | Desktop tabs for common features.
- The project-local screenshot skill.
- Publishing the docs (`publish-docs`).

### Out of scope

- **User docs for experimental features.** They are documented when they graduate, not before.
- **Any code surface in either client, and any daemon or protocol change.** This is a docs PRD.
- **A hook enforcing the screenshot skill.** It is the fallback if the skill proves unreliable, not part of this work.
- **Developer docs.** They stay under `docs/develop/` (CLAUDE.md rule 11), except where the screenshot tool's own page needs a scenario documented.

## Constraints

- **Do not document experimental features.** Only surfaces that have graduated from the `experimental` flag by the time this work runs get user docs (see #1308–#1312 for the current desktop graduations). The docs must say which desktop features are behind `experimental`, per the inventory, so a user who does not see one knows why.
- **Terminology follows the glossary from #1045**: the TUI's words, except where the TUI itself is inconsistent, in which case the user decides.
- User docs go under `docs/`; developer docs stay under `docs/develop/` and are never added to `site/sidebars.js` (CLAUDE.md rule 11).
- No hard-wrapped Markdown prose: one line per paragraph (CLAUDE.md rule 10).
- **CLAUDE.md rule 9 (experimental flag) does not apply.** This PRD adds no user-visible code surface — no pane, field, command, tab, footer or keybinding — so there is nothing to gate and no `show_<feature>()` wrapper to add.
- **CLAUDE.md rule 12 (daemon contract) does not apply.** This PRD changes no daemon, TUI↔daemon protocol, orchestration or hook code, so there is no `PROTOCOL_VERSION` question, no `.breaking.md` fragment and no cross-version manual test.
- Absolutes in the new pages are checked against the code before they are written (CLAUDE.md rule 17), which matters most for the "both clients have it" claims the common group rests on.

## Dependencies and sequencing

- **#1045** (one glossary): must land first, so the docs use settled terms. Closed as of 2026-09-27.
- **#746** (logo): should land first, so screenshots and the app icon do not immediately go stale. Closed as of 2026-09-27.
- **#757** (signed desktop artifacts): the install page describes the signed install. Closed as of 2026-09-27; v0.42.0 shipped the first signed `.dmg`.
- **#1322** (generating screenshots of both clients from code): not a blocker. Closed as of 2026-09-27; `cargo docs-screenshots` is the tool this PRD uses.
- **#1308–#1312** (desktop graduations): not blockers, but they decide which desktop surfaces the docs may cover when the inventory is taken.

Target: the release after the one carrying #1045 and #746.

## Milestones

- [x] **M1** — Desktop feature inventory from the code, checked into this PRD
- [x] **M2** — Docs restructured into common / TUI / desktop, sidebar updated
- [x] **M3** — Desktop pages written for every graduated desktop feature
- [x] **M4** — Install page (absorbs #765)
- [x] **M5** — Home page presents both clients
- [x] **M6** — Screenshots added across the docs via `cargo docs-screenshots`, with TUI | Desktop tabs for common features
- [x] **M7** — Project-local skill for evaluating screenshots on docs changes
- [x] **M8** — Docs published (`publish-docs`) — delivered by `release.yml`'s `docs` job, which publishes `main` on every release — the first release after this merges publishes these docs; no separate `/publish-docs` run

## Risks

- **The inventory goes stale while the pages are written.** Graduations (#1308–#1312) may land mid-work. Mitigation: take the inventory at the start, as the issue requires, and re-check the `experimental` column before publishing.
- **Screenshots go stale.** Accepted by design; generating them from scenarios keeps regeneration a command away.
- **A "common" claim is wrong.** A feature documented as shared that one client lacks misleads users of that client. Mitigation: the inventory's "TUI has it?" column is the source of the common group, and it comes from the code.
- **Agents do not invoke the screenshot skill.** Mitigation: the `docs/**` hook named in section 6, built only if this is observed.

## Success criteria

- Every graduated desktop feature has user docs, and no experimental one does.
- A user can install and first-launch the desktop app on each published platform from the install page alone, and #765 is closed.
- The home page and sidebar present the TUI and the desktop app as two clients of one daemon.
- Common features illustrated with a screenshot show both clients as tabs, and every screenshot is reproducible with `cargo docs-screenshots`.
- An agent editing `docs/` in an unrelated task is prompted by the skill to consider a screenshot.

## Refs

#1322, #176, #765, #757, #746, #1045, #1308, #1309, #1310, #1311, #1312

## Work Log

### 2026-09-27 — Created

Document written from issue #1321's body. The desktop feature inventory is left as a placeholder for M1, as the issue requires.

### 2026-09-27 — M1: desktop feature inventory and docs plan

The [Desktop feature inventory](#desktop-feature-inventory) was taken from `desktop/src/` and `desktop/src-tauri/src/`, with each "TUI has it?" claim checked in `src/`. It lists 17 graduated feature rows and 8 experimental ones. The five experimental surfaces are the five `show_desktop_*` wrappers (#1308–#1312), and three more features are experimental because they exist only on the deck screen (Start / Stop / Replace daemon, Rename, the Output reader). All five graduation issues were still open. #1311 still names the wrapper `show_desktop_workflows`; the code calls it `show_desktop_orchestrations`. Eight terminology questions are left open for the user. One finding changes M4: with the flag off, the desktop starts no daemon. A [Docs plan](#docs-plan) was added: how each page is classified, a sidebar that moves no URL, whether Tabs work, ten screenshot scenarios (two existing, eight new), and the facts about how docs get published. No doc page, sidebar or code was changed.

### 2026-09-27 — M2, M3, M4, M5, M7: docs for both clients

**M2.** `site/sidebars.js` now groups the pages as **Both Clients** / **Terminal UI** / **Desktop App**, with Getting Started, Installation, Troubleshooting and License at the top level; no page moved, so no URL changed. Tabs were proven on the first converted page (`getting-started.md`) with `npm run build` before being used elsewhere: `<Tabs groupId="client">` with `TabItem value="tui"` / `value="desktop"` compiles in the `.md` pages and renders `role="tab"` items in the static output. One gotcha found doing it: pages are compiled as MDX, so a `{name}` in prose is a JavaScript expression and fails the static render (`ReferenceError: name is not defined`); placeholders go in code spans. Desktop tabs were added where a common flow differs per client (getting-started Launching and Basic workflow, session-management statuses, orchestration's start, dispatcher's start, remote-environments' quick start); TUI-only sections are labelled in place (session-management's card details and Resuming Sessions, orchestration's tab navigation, the Schedules dialog, keyboard shortcuts, workspace modes, three troubleshooting sections), without renaming any heading, so no anchor moved. The existing `dashboard` and `dashboard-empty` images are embedded as TUI | Desktop tabs on getting-started, and `dashboard` on session-management; `dashboard-tui.png` replaced the hand capture `getting-started-launching.jpg` on the Launching section, which shows the same screen. The TUI's status for a waiting agent is written as **Needs Input** (what `status_style` shows) rather than `WaitingForInput` (the enum name, which `daemon status` prints).

**M3.** Six pages under `docs/desktop/`: `index.md` (a second client of the same daemon, what the TUI has and it lacks, the experimental surfaces and how the app reads the flag), `dashboard.md`, `new-agent.md`, `daemons.md`, `settings.md` (Appearance, Zoom, the settings file, the desktop's keyboard shortcuts), `voice.md`. Labels, flows and keys were read from `desktop/src/` and `desktop/src-tauri/src/`. Two claims had to be corrected against the code while writing: `Ctrl+N` / `⌘N` works only on the Dashboard with no agent pane open (`AgentOverview.tsx`), not everywhere; and New agent's **Command** *is* pre-filled from the daemon's `default_command` or last command (`seedCommand`, `desktop/src/lib/newAgent.ts`). After a start the app opens the new agent's pane (`onAppeared`), rather than returning to the Dashboard.

**M4.** `installation.md` gained a Desktop app section (absorbs #765): the two assets and which is signed, the provenance check, per-platform install and first launch, how the app gets a daemon, and keeping the app and daemon on one release. Verified for it: the `.deb` (v0.43.0, downloaded and listed with `dpkg -c`/`dpkg -I`) is package `agent-deck`, depends on `libwebkit2gtk-4.1-0` and `libgtk-3-0`, and installs `/usr/bin/dot-agent-deck-desktop` **and** `/usr/bin/dot-agent-deck`; the `.dmg` carries the daemon at `Agent Deck.app/Contents/MacOS/dot-agent-deck` (`release.yml` asserts that path) and puts nothing on `PATH`. **The daemon recipe, checked against the code:** the flag-off app only connects (`connect()` bootstraps with `startIfMissing: false`, and `bootstrap` returns early when `start_if_missing` is false). The idle monitor (`run_idle_monitor`, `src/daemon.rs`) shuts the daemon down 30 s after `clients == 0 && agents == 0 && no enabled schedules`, and arms at a fresh start too, so a `daemon serve` nothing attaches to within 30 s exits; `daemon serve` uses `Daemon::with_attach`, which takes `idle_shutdown_from_env()`, so `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` disables it. Every accepted attach connection increments the client count (`ClientGuard`, `src/daemon_protocol.rs`), and the desktop holds a persistent `EventSubscription` per observed daemon (`daemon_bridge.rs`), so a daemon the desktop is attached to does not idle out. A TUI-started daemon survives the TUI's Detach while agents run or another client (the desktop) is attached, and otherwise exits 30 s later; the quit dialog's Stop shuts it down. Remote daemons follow the same rule on their host, so `daemons.md` and `remote-environments.md` say how to keep one up. `troubleshooting.md`'s Hooks section said the desktop app "starts only the daemon bundled inside it", which is false with the flag off; it now says the daemon the app connects to installs the hooks, however it was started. The v0.43.0 release notes carry the same claim ("it starts the background daemon bundled inside it") and are not editable here.

**M5.** The home page gained a "Two clients, one daemon" section (`clients` in `site/src/data/landing-content.js`, rendered in `site/src/pages/index.js`, styled in `index.module.css`) with the `dashboard` image of each client; the tagline (also in `site/docusaurus.config.js`) and hero text name both clients; the desktop band links the install section and says the app needs a running daemon; the docs door list has a Desktop app card.

**M7.** Project-local skill `.claude/skills/docs-screenshots-review/SKILL.md`, listed in CLAUDE.md rule 13. It triggers on any edit under `docs/` outside `docs/develop/`, and on a UI change that makes a docs screenshot stale; it has the agent decide whether a screenshot is warranted, produce it with `cargo docs-screenshots` (one scenario name on both clients, embedded as TUI | Desktop tabs) or `run-dot-agent-deck`, or tell the user exactly what to capture. The `docs/**` hook fallback is named and not built.

**For M6.** The committed desktop images (`dashboard-desktop.png`, `dashboard-empty-desktop.png`) show older labels than the code: "Agent overview", "Local deck", "DECKS" and "The deck is healthy…", where the code now says "Agent dashboard", "DAEMONS" and "The daemon is healthy…" (#1045). The page text follows the code; regenerating those images fixes the mismatch. The pages written here reference only the four existing images; the new scenarios are embedded in M6.

### 2026-09-27 — M6: screenshots embedded

Every image in the Docs plan's screenshot table is on its page. **Both clients, as TUI | Desktop tabs:** `new-agent` in getting-started's Basic workflow; `orchestration` in orchestration.md's "Starting an orchestration tab". **Single images:** `new-agent-desktop.png` on `desktop/new-agent.md`; `dashboard-fleet-desktop.png` (One section per daemon) and `agent-pane-desktop.png` (The agent pane) on `desktop/dashboard.md`; `settings-daemons-desktop.png` on `desktop/daemons.md` and in remote-environments' Desktop tab; `settings-voice-desktop.png` on `desktop/voice.md`; `schedules-tui.png` on `scheduled-tasks.md`; `help-tui.png` on `keyboard-shortcuts.md`. getting-started's Orchestration section, which is not tabbed, carries both `orchestration` images one after the other. `orchestration-tui.png` replaced the hand capture `orchestration-start.png` in both places it was used (getting-started and orchestration.md); the file stays in `site/static/img/` and no page references it now. The other hand captures stay. The `settings-daemons` image shows the form before **Test connection** is pressed, and its alt text says so, because a successful result is not reachable in fixture mode. `desktop/new-agent.md`'s `default_dir` link now points at `configuration.md#default-directory`. `npm run build` in `site/` passes.

**M8** needs no separate step: `release.yml`'s `docs` job (`needs: [prepare, finalize]`, no `if:`) calls `docs-publish.yml` on every release run, and that workflow's `publish` job checks out `ref: main`. So M8 is delivered by the first release cut after this PRD's branch merges to `main`; `/publish-docs` is only for publishing sooner.

### 2026-09-28 — Late screenshot fixes and a Schedules dialog bug

**Screenshots (`983d60b1`).** The `orchestration` TUI scenario now shows both roles at work, not a tab on launch: the planner and builder cards are Working, each with its prompt and a tool line, and read exactly `Last: 2s` and `Last: 3s`. It uses the same pane-addressed hook events, whole-second ages and fresh-sandbox retry as `dashboard`, and captures 18 seconds after staging so the 15-second activation banner has gone (`docs/develop/docs-screenshots.md` records this). The `dashboard-fleet` desktop scenario now switches the daemon selector to **All daemons** before capturing, so the image shows both daemon groups. The alt text for `orchestration-tui.png` in `getting-started.md` and `orchestration.md` was then rewritten to match the new image: the `demo-loop` tab, both roles Working with their tools and ages, and the planner selected with its pane on the right.

**Skill and alt text (`23b17ad8`).** The `run-dot-agent-deck` fallback in the `docs-screenshots-review` skill now carries the redaction rule from `docs/develop/docs-screenshots.md`: redact before writing, inspect the final image, and do not commit one that cannot be made clean. In `desktop/dashboard.md`, the agent pane's alt text and the sentence next to it now say that its header shows the agent type, or the orchestration role, and not the display name.

**Schedules dialog bug (`3a367f87`).** Generating the `schedules` screenshot (whose schedule name was made realistic in `63fda1af`) showed a TUI defect: `render_scheduled_tasks` padded the NAME cell only to the widest name, so the header read `NAMESTATUS` and the longest name ran into its status (`nightly-triagedisabled`). Fixed in this PR with a two-cell gutter after NAME; the modal width derives from the same column width, so it stays consistent. Covered by the unit test `ui::tests::schedule_manager_separates_name_and_status_columns`, and user-visible, so it has a fragment, `changelog.d/1321.bugfix.md`. `schedules-tui.png` was regenerated after the fix (`68b03097`).

**Tests and reel.** The branch adds no `#[spec]` tests and no `[reel]` entries in `tests/CATALOG.md`, so there is no recorded run and no demo reel clip for this PRD.

**M8** stays open, for the reason given in the M6 entry: `release.yml`'s `docs` job publishes the docs with the first release cut after this branch merges to `main`.

### 2026-09-28 — Archived

All eight milestones complete. M8 is delivered by `release.yml`'s `docs` job, which publishes `main` on every release — the first release after this merges publishes these docs; no separate `/publish-docs` run is needed. PRD moved to `prds/done/`.

### 2026-09-28 — Home page story rows cover both clients

A review of the PR found that the home page's "How it works" rows (01–04) still described only the terminal UI, in their copy and in all four frames; M5 had added a separate two-clients section and left them alone. Their copy now covers both clients: row 01 is "Start an agent" and names both Ctrl+n and New agent, and rows 02–04 name the card and the tab alongside the row and the group. Each step now puts its text above both clients' screenshots, side by side and labelled **Terminal UI** and **Desktop app**, so the home page shows both at once instead of asking the reader to pick one. A switch that shared its choice with the docs' tabs was tried first and replaced at the user's request, since choosing a client suits the docs rather than the home page. Rows 01–03 use the generated desktop frames `new-agent`, `dashboard-fleet` and `orchestration`. Row 04 has no desktop frame, because the desktop fixture has no dispatcher transcript to capture, so it shows the terminal UI frame alone and its caption says what the desktop app does there. The `tall` and `wide` layout flags that kept the old two-column rows in step went with that layout.

The rest of the home page follows the same rule, which the user stated as general: **show and explain both clients, unless a feature works in only one of them.** The "Why" paragraph, the "Who this is for" list and three of the four principles described only the terminal UI; they now name both, and the one genuinely different trait is said per client ("Keyboard, pointer or voice"). The late "There is a desktop app too. It is an alpha." band no longer made sense on a page that presents both clients from the top; its content was install detail, so it moved into "Runs where you work": a Desktop app row in the platform list (mirroring `docs/installation.md`), a Desktop app route with both assets and a link to its install steps, and the provenance check under both columns, since it covers every release asset. The rule itself is recorded in the `docs-screenshots-review` skill, whose trigger now also covers the home page's source.

The hero followed: its single terminal UI frame is now a pair, the terminal UI's orchestration capture beside the desktop app's `agent-pane` frame, each in its own window with its client named in the window bar. The two columns are sized in the frames' aspect ratios (about 3:1 and 16:10), so the windows stand the same height without cropping either, and they stack on narrow screens. The cost is size: at the full page width the terminal UI frame is about 250px tall instead of about 390px.
