# PRD #1321: User docs for both clients — common, TUI and desktop sections, with screenshots

**Status**: In progress — M1 done (2026-09-27)
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
- **Screenshots come from `cargo docs-screenshots`** (issue #1322, [`docs/develop/docs-screenshots.md`](../docs/develop/docs-screenshots.md)), not from hand captures. Each new screenshot is a new or extended scenario in that tool's registry (`xtask/screenshots/src/scenarios.rs`, with its TUI capture in `tests/e2e_docs_screenshots.rs` and its desktop capture in `desktop/screenshots/desktop.shot.ts`), so it can be regenerated when the UI changes. A feature both clients have uses the **same** scenario name on both, depicting the same state, which is what makes the TUI | Desktop tabs possible (`dashboard` is the worked example). A screen the tool genuinely cannot reach is the exception, and is recorded as such where it is used.

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
- [ ] **M2** — Docs restructured into common / TUI / desktop, sidebar updated
- [ ] **M3** — Desktop pages written for every graduated desktop feature
- [ ] **M4** — Install page (absorbs #765)
- [ ] **M5** — Home page presents both clients
- [ ] **M6** — Screenshots added across the docs via `cargo docs-screenshots`, with TUI | Desktop tabs for common features
- [ ] **M7** — Project-local skill for evaluating screenshots on docs changes
- [ ] **M8** — Docs published (`publish-docs`)

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
