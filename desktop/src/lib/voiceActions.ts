import type { CardTabSection, DeckView, InboxActionVerb } from "../types";
import type { VoiceDeckIdentityDto } from "./bridge";
import { clearDashboardFilter, setDashboardFilter, type DashboardFilter } from "./dashboardFilter";
import type { CardStep, OverviewMode } from "./agentCards";
import type { CardReferenceTarget } from "./prBrowser";
import type { InterruptedDialogControls } from "./interruptedWork";

/**
 * PRD #802 M2 — the frontend action registry, and the app's one dispatch seam
 * for the rail and the command palette.
 *
 * # Why this exists at all
 *
 * The voice command table's `invoke` column names an entry **here**, not a
 * `#[tauri::command]`. That correction is PRD #802's most consequential one and
 * it was measured rather than assumed: none of the registered Tauri commands
 * performs a navigation, because navigation in this app is React state —
 * `setView`, a `useState` boolean, a selected agent id. Voice "produces an
 * action and dispatches it where a click dispatches one", and a click is
 * dispatched in the frontend. (The count that used to sit in that sentence is
 * gone on purpose: a number in a comment is read as a property. It said
 * *thirteen*, and `generate_handler!` had already grown past that — recount it
 * there if you need the number, rather than trusting one written here.)
 *
 * So an entry is **not** a description of a control. It IS the control: the rail
 * buttons, the palette items, the agent tile's open/close pair, the overview's
 * row and — since `stopVoice` — the Voice button itself all run through
 * `VOICE_ACTIONS[id].run(...)`. That is the
 * property that earns the capability definition M3's guard checks — an entry
 * cannot be deleted without breaking a control, and a control reachable from the
 * rail or the palette cannot exist without an entry. A registry beside the app
 * rather than inside it would be a checked-in list under a better name.
 *
 * # The seam is the RAIL, the PALETTE, the VOICE SURFACE and the voice-reachable entries
 *
 * (It said *four* voice-reachable entries, and was already wrong by one before
 * this PR added more. The heading no longer counts them for the reason the
 * paragraph above gives about numbers in comments.)
 *
 * **It used to say "one capability, one dispatch path" while that was false**,
 * and then carried a list of the `no_voice` capabilities with a second,
 * in-panel `setState` path in `App.tsx` (PRD #802's deferred D10): the agent
 * tile's own selection (`focusAgent`), the workspace header's Events button
 * and an evidence row's select-and-open (`toggleEvidenceDrawer`), the run
 * graph's Edit loop controls and Projects' Configure orchestration
 * (`openOrchestrationOrder`), the orchestration editor's Choose one (`openProjects`), and
 * the empty deck's Configure agents (`openAgentProfiles`).
 *
 * **PRD #1195 M1 closed that residual**: every one of those controls now
 * dispatches through `VOICE_ACTIONS[id].run(...)`, and `voiceActions.test.ts`
 * proves each one crosses the registry and still has its visible effect. It
 * was done not to make any of them voice-reachable — each keeps its `no_voice`
 * reason — but because "a user-facing control dispatches through the registry"
 * is only a rule a build can check once it has no sanctioned exceptions.
 *
 * Where a call site needed more than an entry offered, the entry was widened
 * rather than the control narrowed: an evidence row SHOWS the drawer on the
 * item it selects and never hides it, so `toggleEvidenceDrawer` takes an
 * optional direction instead of the row becoming a flip.
 *
 * **What still writes that state directly is a dismissal or an invariant, not
 * a control opening a capability**: a panel's own close, the launch flow
 * closing the editor it launched from, the deck keeping its selection on a
 * live agent. That line is drawn by `xtask/linkage-check` rule 18
 * (`voice_capability_state.rs`, PRD #1195 M2): a setter named in an action
 * context may be written elsewhere in the app shell only as a dismissal or
 * under a written `voice-registry-exempt:` reason, and every `useState` there
 * is registry-owned or carries one. Its own doc comment says what it cannot
 * see — state outside the files it scans, and a capability reached through
 * anything other than a `set*` identifier.
 *
 * # What the guard needs from this file, and what it will refuse
 *
 * `xtask/linkage-check` rule 14 reads this file as text, the way
 * `desktop_settings_secrets.rs` already reads `bridge.ts`. That is only sound
 * while the literal below is **statically readable**, so the rule refuses a
 * computed key or a spread among the entries — exactly the refusal
 * `normalizeDesktopSettings` already carries, and for the same reason. Keep the
 * object a plain literal with plain keys. A spread *inside* an entry's own body
 * is fine; the rule looks only at the registry's own top level.
 *
 * # Every entry is classified, and `no_voice` is a sentence rather than a flag
 *
 * Each entry carries either `voice: true` — meaning a row in
 * `desktop/src-tauri/src/voice/commands.toml` names it — or a non-empty
 * `no_voice` reason. Never both, never neither. The reason is read by a person,
 * so "not yet" is not one: it says what about the capability makes it a poor
 * fit for a spoken command **today**, which is what lets a later reader
 * disagree with it.
 *
 * # What this is NOT
 *
 * It is not an inventory of everything the app can do. The guard proves every
 * entry here is classified; it cannot discover a capability that never routes
 * through this file, and there were 80 `onClick=` sites in non-test `.tsx` under
 * `desktop/src` when this was written. Reading the guard as completeness is
 * PRD #802's own named risk.
 *
 * Issue #1701 narrowed that gap without closing it: linkage-check rule 14's control-site
 * ratchet lists each `onClick`-family handler that is not a single
 * `VOICE_ACTIONS` dispatch (and does more than stop its event) in
 * `xtask/linkage-check/voice-control-sites.toml`, with a reason, and fails the
 * build on a new one. So a NEW control either goes
 * through an entry here or somebody writes down why it does not.
 */

/**
 * The overlays the deck can put over itself.
 *
 * Deliberately not part of {@link DeckView}: these are `useState` booleans
 * inside `ControlDeck`, so *"an overlay is open"* is not a screen and a
 * command's `screens` column cannot depend on it. PRD #802 says so, and a row
 * that needed it would be a change to where the state lives rather than a
 * change to the table.
 */
export type DeckOverlay = "projects" | "profiles" | "orchestration" | "settings";

/** Issue #1492 — where a spoken scroll moves the agent dashboard: a screen down or up, or to either end. */
export type DashboardScroll = "down" | "up" | "top" | "bottom";

/** Which agent's pane to open, and which screen it is opened over. */
export type AgentViewTarget = {
  deckId: string;
  agentId: string;
  from: "deck" | "overview";
  /**
   * PRD #1666 part 2 (G-fix6): the daemon process that listed the agent, from
   * an opener that holds its record. Absent where only the ids are known (a
   * row resolved by number); the shell then binds the view to the record
   * listed when it is opened.
   */
  incarnation?: string;
  /** PR #2491 decision 2026-11-26 — the pane opens on its Card tab: only where the user asked for the card itself (a card's `+N`, voice's "show the coder's card"). */
  cardTab?: boolean;
  /** …scrolled to this part of the Card tab. */
  cardSection?: CardTabSection;
};

/** One agent, named the way every caller here names one. */
export type AgentTarget = { agentId: string };

/**
 * Everything a registry action can ask the app to do.
 *
 * No entry takes the whole of it. Each `run` takes a `Pick` of exactly the
 * members it uses, so the type says what a control needs and a host that cannot
 * serve a member cannot dispatch the action that wants it. `DeckShell`'s
 * overview branch builds two members; `DeckSurface` builds all of them.
 *
 * **Optionality is the HOST's, not the registry's.** `DeckSurface` takes
 * `onNavigate` and `onCloseAgent` as optional props and has done since PRD
 * #1105, so its context forwards to them optionally. That keeps today's
 * behaviour exactly — a deck mounted without a navigator still renders, and its
 * Overview button still does nothing — instead of making the registry the place
 * that decides what an absent prop means.
 */
export type VoiceActionContext = {
  /** Replace the mounted screen, or overlay an agent's pane on it. */
  navigate: (view: DeckView) => void;
  /** Close an open agent view, back to the screen it was opened from. */
  closeAgentView: () => void;
  /** Put one overlay over the deck. Leaves the others exactly as they were. */
  openOverlay: (overlay: DeckOverlay) => void;
  /** Close every overlay, leaving the deck itself. */
  closeOverlays: () => void;
  /**
   * Flip the evidence drawer — or, given `open`, put it that way round. The
   * direction is what an evidence row's select-and-open needs: it shows the
   * drawer whichever way it was pointing, which a bare flip cannot promise.
   */
  toggleEvidence: (open?: boolean) => void;
  /** Make one agent the deck's selected tile. */
  selectAgent: (agentId: string) => void;
  /**
   * Issue #1492 — scroll the agent dashboard so one agent's row is on screen,
   * before its pane opens over it: saying the number of a row scrolled out of
   * view acts on that row and leaves it in view behind the pane. Served by the
   * overview; the deck has no rows to reveal.
   */
  revealAgent: (deckId: string, agentId: string) => void;
  /**
   * Issue #1492 — scroll the agent dashboard by about one screen, or to either
   * end. Answers `undefined` when it scrolled, or the sentence saying why not
   * (the whole dashboard is on screen, or it is already at that end).
   *
   * **Served by the OVERVIEW**, whose screen it scrolls. "next page" and
   * "previous page" reach it too while nothing over the dashboard pages (the
   * shell's `turnPage`), since the dashboard is not split into pages.
   */
  scrollDashboard: (move: DashboardScroll) => string | undefined;
  /** Select an agent, show its terminal, and ask that terminal for the caret. */
  focusTerminal: (agentId: string) => void;
  /** Move the deterministic fixture loop one node. Fixture mode only. */
  advanceFixture: () => void;
  /**
   * Turn voice control off: release the microphone and stop listening.
   *
   * **Served by the VOICE SURFACE rather than by a screen**, which is what
   * makes it servable everywhere — see {@link VoicePanelChannel}. The Voice
   * button and this are the same control by two routes, exactly as the rail
   * button and `openOverview` are.
   */
  stopVoice: () => void;
  /**
   * Open the overlay listing what can be said on this screen (PRD #802 D7).
   *
   * Served by the voice surface for {@link stopVoice}'s reason, and callable
   * everywhere for a sharper one: a user who does not know what to say cannot
   * be told to go somewhere else first.
   */
  showVoiceCommands: () => void;
  /**
   * Type one utterance's words into the open agent's prompt (PRD #802 D6,
   * rebuilt).
   *
   * **One utterance, not a mode.** What shipped first aimed the microphone at
   * an agent and typed everything until an exit phrase was heard; this types
   * what it was given and is finished. The text is on `target.text`, taken
   * Rust-side from the transcript itself — never from the model's answer.
   *
   * Served by the voice surface rather than by a screen, because the countdown
   * to a send that follows it is state no screen has anywhere to keep.
   */
  typeIntoAgent: (target: VoiceDispatchTarget) => void;
  /** Press Enter in the open agent's prompt. The voice surface's, for
   * {@link typeIntoAgent}'s reason — it also cancels the pending countdown,
   * which only the surface holds. */
  submitAgentPrompt: (target: VoiceDispatchTarget) => void;
  /**
   * Enter the dictation mode for the pane on screen (PRD #1260): every later
   * utterance is typed into that agent's prompt until the mode ends.
   *
   * Served by the voice surface, because the mode is the surface's own state —
   * one of its panel states, beside a pending send — and it refuses entry,
   * through {@link reportRefused}, on a pane that cannot take input.
   */
  startDictation: (target: VoiceDispatchTarget) => void;
  /** Leave the dictation mode. Sends nothing: what was typed stays in the
   * prompt. The voice surface's, for {@link startDictation}'s reason. */
  stopDictation: () => void;
  /**
   * PRD #1541 — interrupt the open agent's turn with the deck's own interrupt
   * key, clear its prompt, or remove the last dictated words. The voice
   * surface's, because what they need — the words voice typed into the
   * prompt since it was last sent, whether anything else was typed there, and
   * when the agent was last interrupted — is state only the surface keeps.
   * Reached only from typing mode's local intercept.
   */
  interruptAgent: (target: VoiceDispatchTarget) => void;
  clearAgentPrompt: (target: VoiceDispatchTarget) => void;
  scratchLastDictation: (target: VoiceDispatchTarget) => void;
  /**
   * PRD #1497 decision 2 of 2026-10-09 — turn Settings → Voice → Reading on,
   * from any screen: from then on the app speaks a short summary of each turn
   * every agent on the deck being viewed finishes, and their permission
   * prompts and errors. The voice surface's, because it reads and speaks, and
   * it saves the switch through the same save as Settings.
   */
  startReading: () => void;
  /** Turn the Reading switch off. The voice surface's, for {@link startReading}'s reason. */
  stopReading: () => void;
  /** PRD #1497 D6 — silence the app's speech now and drop what is waiting.
   * Reading mode stays on. The voice surface's: the speech queue is its own. */
  quietSpeech: () => void;
  /**
   * Close the voice surface's own overlay.
   *
   * **Published only while that overlay is OPEN**, and that is the whole
   * mechanism: *"an overlay is open"* is not expressible in `screens` — those
   * booleans are not in `DeckView` — so the fact travels as the presence or
   * absence of this member, read at dispatch time by {@link closeTopmost}.
   */
  dismissVoiceOverlay: () => void;
  /** Say that there was nothing on top to close. The voice surface owns every
   * sentence about itself, so the honest answer to `close` with nothing open
   * is written there and not composed here. */
  reportNothingToClose: () => void;
  /**
   * Say that a dispatch reached something that would not do it, in that
   * thing's own words: `close` finding a dialog that may not be closed yet
   * (PRD #1223 U5), or a directory row finding that the browser has moved on
   * since the utterance was declared (PRD #1223). The voice surface renders it
   * where it renders {@link reportNothingToClose}'s sentence; the sentence
   * itself belongs to whatever refused.
   */
  reportRefused: (reason: string) => void;
  /**
   * PRD #1401 — say what a dispatch whose effect settles LATER is doing, and
   * then how it ended: `pending` now, in place of the resolver's sentence
   * (which was written before anything ran, and would claim a success nobody
   * has seen yet), then `done` when `work` resolves to `undefined`, or the
   * sentence it resolves to when it did not happen. A later report is
   * dropped once the user has said something else, whose report it must not
   * cover. `work` resolves rather than rejects, so a caller with no voice
   * surface (the toolbar's button) leaves no rejection unhandled.
   */
  reportPending: (pending: string, work: Promise<string | undefined>, done: string) => void;
  /**
   * PR #2491 decision 2026-11-25 — ask which one, the way a spoken name that
   * fits several agents is asked about: the voice panel offers `ask`'s
   * candidates as the numbered "Which daemon?" choice, and the row being
   * dispatched runs again with the one chosen as its `param`. Said as a
   * refusal (`ask.sentence`) where no choice can be offered (a confirmation
   * open, too many candidates, or a press rather than speech).
   */
  askVoiceChoice: (ask: VoiceChoiceAsk) => void;
  /**
   * Close the New agent dialog (PRD #1223 U5) — every close route the dialog
   * has, reached by voice. Answers `undefined` when it closed, or the dialog's
   * own sentence when it would not: while a start is in flight every route is
   * blocked (audit F5), and voice is blocked the same way rather than
   * differently.
   *
   * **Published by the overview only while the dialog is OPEN**, for
   * {@link dismissVoiceOverlay}'s reason: *"the dialog is open"* is a
   * `useState` in the overview, not a `DeckView`, so {@link closeTopmost}
   * reads it as this member's presence. It is NOT a way to open or drive the
   * dialog — that is `openNewAgent`, the `open_new_agent` row.
   */
  closeNewAgent: () => string | undefined;
  /**
   * Store `selection` — the Deck selector's token: `local`, or a configured
   * deck's row id — as the deck the app shows (PRD #1195 M3), through the
   * selector's own write. Answers `undefined` when it did, or when that deck
   * was already the one shown, which writes nothing; otherwise the sentence
   * saying why not (a token the selector no longer lists).
   *
   * **Served by the SHELL**, like {@link closeSettings}: the selector sits on
   * the deck and on the overview, and the settings document it writes is the
   * shell's. The menu itself builds the same member over the same function
   * (`chooseDeckSelection` in `DeckSelector.tsx`).
   *
   * `identity` is the row's address as voice resolved it
   * ({@link VoiceDispatchTarget.deckIdentity}); a row whose address no longer
   * matches is refused rather than switched to. The menu passes none: it reads
   * the row it writes in the same render.
   */
  switchDeck: (selection: string, identity?: VoiceDeckIdentityDto) => string | undefined;
  /**
   * PRD #2274 — open the Add a daemon dialog over the screen that is up: the
   * add form Settings → Daemons uses, in a dialog, so finishing or cancelling
   * returns to that screen.
   *
   * **Served by the SHELL**, for {@link switchDeck}'s reason: the selector's
   * Add a daemon… entry sits on the deck and on the overview, and the dialog
   * writes the shell's settings.
   */
  openAddDaemon: () => void;
  /**
   * PR #1451 round 3, change 4 — turn the page of the list on screen that is
   * split into pages while voice is on: the New agent dialog's while it has
   * one, else the Daemons screen's. On the agent dashboard, which scrolls
   * instead of paging (issue #1492), it scrolls by about a screen. Answers `undefined`
   * when it turned, or the sentence saying why not (nothing pages, or this is
   * the last or the first page).
   *
   * **Served by the SHELL**, which holds the registry every paged list
   * publishes to (`useVoicePages`), for the reason {@link switchDeck} is.
   */
  turnPage: (delta: 1 | -1) => string | undefined;
  /**
   * Close the Settings sheet (issue #1197).
   *
   * **Served by the SHELL, and only while Settings is OPEN with nothing drawn
   * over it**, for {@link dismissVoiceOverlay}'s reason: Settings is a
   * shell-level overlay boolean (`useShellOverlays`), not a `DeckView`, so
   * {@link closeTopmost} reads *"Settings is on top"* as this member's
   * presence. The shell and not a screen, because the sheet opens over the
   * overview as well as the deck. What is drawn over it is the fence's stack
   * (`useInertBackground`), which orders the app's overlays as they paint.
   */
  closeSettings: () => void;
  /**
   * PR #1680 review — close the topmost of the app's overlays when it is one
   * with no member of its own: the deck's command menu, its shortcut sheet, a
   * configuration sheet, or a confirmation or upgrade dialog (issue #1709).
   * Answers the sentence saying why when it cannot close yet — an upgrade
   * still running — and `undefined` (or nothing) when it closed.
   *
   * **Served by the SHELL, and only while such an overlay is on top**, read
   * from the fence's stack (`useInertBackground`) at dispatch time, for
   * {@link dismissVoiceOverlay}'s reason.
   */
  closeOverlay: () => string | void;
  /**
   * Open the New agent dialog (PRD #1223), with `deckId` preselected when the
   * control that opened it belongs to one deck — a deck group's header.
   *
   * **Served by the OVERVIEW alone**, the screen the flow lives on and the one
   * whose fleet the deck step lists; see {@link VoiceOverviewContext}. The deck
   * screen does not offer it, so a dispatch there is refused against `needs`
   * rather than attempted. The overview publishes it only while the dialog is
   * closed, for the same reason `Ctrl+N` stands down while it is open.
   */
  openNewAgent: (deckId?: string) => void;
  /**
   * PR #1672 — open one daemon's Host figures: the popover its **Host** button
   * beside the daemon's name opens (PRD #1258). `deckId` is the daemon a
   * row's `deck_ref` resolved to, or the button's own; without one, the one
   * daemon on the dashboard that shows a Host button. Answers `undefined` when
   * it opened, or the sentence saying why not: no daemon named and several
   * shown, a named daemon that is not connected, or none to show.
   *
   * **Served by the OVERVIEW**, which renders the buttons and holds which
   * popover is open; see {@link VoiceOverviewContext}.
   */
  showHostMetrics: (deckId?: string) => string | undefined;
  /**
   * PR #1672 — close the open Host popover: what its X (`Close host`), Escape
   * and a press outside it do.
   *
   * **Published by the overview only while a popover is OPEN**, for
   * {@link dismissVoiceOverlay}'s reason: *"a Host popover is open"* is a
   * `useState` in the overview (`hostOpen`), not a `DeckView`, so
   * {@link closeTopmost} reads it as this member's presence. Opening one is
   * {@link showHostMetrics}, the `show_host_metrics` row.
   */
  closeHostMetrics: () => void;
  /**
   * The agent dashboard's counters (issue #2312 follow-up): the one button in
   * the top bar that replaced the row of AGENTS, RUNNING, NEEDS INPUT … DAEMONS
   * instruments, and the popover it opens with every one of them. Opening it
   * is what the button's click does, and what the `show_dashboard_counters`
   * row dispatches. **Served by the OVERVIEW**, which holds whether it is open.
   */
  showDashboardCounters: () => void;
  /**
   * Close the counters popover: what a second click on the button, Escape and
   * a press outside it do. Published only while it is open, for
   * {@link closeHostMetrics}'s reason: `closeTopmost` reads its presence.
   */
  closeDashboardCounters: () => void;
  /**
   * PRD #1790 stage 2 — open a daemon's "Interrupted work" dialog: what its
   * banner's Show does. `deckId` is the daemon a row's `deck_ref` resolved
   * to, or the banner's own; without one, the connected daemon when there is
   * one, and with several, the one that lists work once each has been asked
   * again (final reviewer 2). Answers `undefined` when it opened, or the
   * sentence saying why not — later, as a promise, when it asked again first.
   * **Served by the OVERVIEW**, which renders the banners.
   */
  showInterruptedWork: (deckId?: string) => string | undefined | Promise<string | undefined>;
  /** PRD #1790 stage 2 — hide a daemon's interrupted work banner until it is another daemon process: records nothing. */
  hideInterruptedWork: (deckId: string) => void;
  /**
   * PRD #1790 stage 2 — the open "Interrupted work" dialog's calls: reclaim,
   * dismiss and dismiss all by every route, and its own buttons. Published by
   * the overview while the dashboard is up; while no dialog is open each
   * action answers the sentence that says to open it first.
   */
  interruptedDialog: InterruptedDialogControls;
  /**
   * PRD #1223 — the New agent dialog's directory browser, by voice: go into
   * the child a `dir_ref` resolved to (`target.directoryPath`), go up to `..`,
   * or choose the directory on screen. Each calls the function the browser's
   * own key calls, and each answers `undefined` when it acted or the dialog's
   * sentence when it would not — see {@link VoiceDispatchTarget.declaredDirectories}.
   *
   * **The first members that reach INSIDE a mounted dialog**, which is the new
   * shape and the reason they refuse rather than merely run: the row was judged
   * callable against what the webview DECLARED with the utterance, and the
   * browser can move during the round trip (a click, a key, a listing landing).
   * So each re-checks the declaration against the live browser and refuses
   * when the two differ, instead of acting on a listing the user has left.
   *
   * Served by the overview, which owns the dialog; see {@link VoiceOverviewContext}.
   */
  openDirectory: (target: VoiceDispatchTarget) => string | undefined;
  goToParentDirectory: (target: VoiceDispatchTarget) => string | undefined;
  useThisDirectory: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * PR #1451 round 3, change 5 — the browser's Filter box: set it to
   * `target.filterText`, or empty it, through the path a keystroke in the box
   * takes. Each re-checks the browser the utterance was judged against, as the
   * moves above do, and answers a refusal in the dialog's words.
   */
  filterDirectories: (target: VoiceDispatchTarget) => string | undefined;
  clearDirectoryFilter: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * PRD #1223 — the rest of the New agent form by voice: choose the Mode chip
   * a `mode_ref` resolved to (`target.modeId`), set Command to the default
   * command of the agent an `agent_type_ref` resolved to
   * (`target.agentTypeId`), or set Name to the words after the marked
   * boundary (`target.text`). Mode and Name call the function the control's
   * own click or keystroke calls; the agent has no control since the Agent
   * picker was removed, and fills Command from the deck's registry. Each
   * answers `undefined` when it acted or the dialog's sentence when it would
   * not — the form can move during the round trip exactly as the browser can,
   * so each re-checks {@link VoiceDispatchTarget.declaredForm} against the
   * live form first.
   *
   * Command takes the user's own words since PR #1451 round 4 (decision D8):
   * `setNewAgentCommand` sets it to {@link VoiceDispatchTarget.commandText},
   * through the path a keystroke in the field takes, so it counts as an edit.
   * Filling it starts nothing — Start is still a separate act.
   */
  chooseNewAgentMode: (target: VoiceDispatchTarget) => string | undefined;
  chooseNewAgentType: (target: VoiceDispatchTarget) => string | undefined;
  nameNewAgent: (target: VoiceDispatchTarget) => string | undefined;
  setNewAgentCommand: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * Issue #1263 — the New agent dialog's deck field, by voice: choose the deck
   * a `deck_ref` resolved to (`target.preselectDeckId`) through the function a
   * click on its row calls. Answers `undefined` when it chose, or the dialog's
   * sentence when it would not (a start in flight, a deck that left the list or
   * can no longer take a spawn). Served whenever the dialog is open — choosing
   * a deck is how its form becomes live — so it does not read `declaredForm`.
   */
  chooseNewAgentDeck: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * Issue #1247 — the dialog's Discard, by voice: close it and keep nothing.
   * Every other close keeps the form as a draft, so this is the one voice
   * member that loses what was typed, and its row is held to the whole
   * utterance. Answers the dialog's sentence while a start is in flight.
   */
  discardNewAgent: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * The New agent dialog's start, by voice — the Start button, acting on the
   * form as it is, which the user is looking at: it starts at once, or answers
   * what is missing. **It used to open a confirmation**; PRD #802 D5's start
   * half was revisited on 2026-09-23 (PRD #1223), for the reasons recorded
   * there and in `docs/develop/voice-first-design.md`.
   */
  startNewAgent: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * PRD #1885 — approve the project commands the `Review project commands`
   * dialog shows, and start the orchestration it was opened for: its Approve
   * and start button. It approves exactly the text on screen (the dialog
   * carries its digest), so it can approve nothing the user is not looking at.
   * Answers `undefined` when it approved, or the sentence saying why not (no
   * review is open, or an approval is already under way).
   *
   * **Served by the SHELL**, from the one review the app can show at a time
   * (`approveShownReview` in `lib/projectTrust.ts`): the dialog opens over the
   * New agent dialog on the overview and over the Runs screen on the deck.
   */
  approveProjectCommands: () => string | undefined;
  /**
   * PRD #1885 — the review dialog's Cancel: close it, starting and storing
   * nothing. Voice reaches it through `close` ("cancel"), as the topmost
   * overlay, and not through a row of its own.
   */
  cancelProjectCommands: () => void;
  /**
   * Issue #2511 — the "approval waiting" marker: open the review a deck asked
   * for over whatever holds it back (an agent's pane, a dialog). Answers
   * `undefined` when it did, or the sentence saying no approval is waiting.
   *
   * **Served by the SHELL** (`showHeldTrustRequest` in `lib/trustRequests.ts`),
   * whose `TrustRequestGate` shows the marker over every screen.
   */
  showApprovalRequest: () => string | undefined;
  /**
   * PRD #2174 — the star reminder's three buttons: Star (open the repository
   * page and stop asking), Later (ask again after another ten launches) and
   * Don't ask again. Each answers `undefined` when it chose, or the sentence
   * saying why not: the reminder is not showing, or something is drawn over
   * it.
   *
   * **Served by the SHELL**, which shows the reminder at startup over
   * whichever screen comes up (`chooseShownStarPrompt` in
   * `lib/starPrompt.ts`).
   */
  starRepo: () => string | undefined;
  remindStarLater: () => string | undefined;
  dismissStarPrompt: () => string | undefined;
  /**
   * PRD #211 — the Antigravity experimental notice's three buttons: Open
   * issues (open the repository's new-issue page), Later (show it again at
   * the next interval) and Don't show again. Each answers `undefined` when it
   * chose, or the sentence saying why not: the notice is not showing, or
   * something is drawn over it.
   *
   * **Served by the SHELL**, which shows the notice after a New agent start
   * of Antigravity (`chooseShownAntigravityNotice` in
   * `lib/antigravityNotice.ts`).
   */
  openAntigravityIssues: () => string | undefined;
  remindAntigravityLater: () => string | undefined;
  dismissAntigravityNotice: () => string | undefined;
  /**
   * PRD #802 D5 — the two voice members that lead to a STOP, and the one rule
   * they share: **each can only OPEN a confirmation.** Neither runs a deck
   * action; the confirmation's own button does, pressed by the user. So a
   * misheard or over-confident answer costs a dialog the user dismisses, never
   * an agent stopped that nobody meant to stop. They keep it where the start
   * did not because they are destructive, their target may be off-screen, and
   * an orchestration close takes several roles at once.
   *
   * - `confirmStopAgent` — the overview row's Stop, for the agent an
    *   `agent_ref` resolved to, or the open pane's agent: the same confirmation
    *   those buttons open.
   * - `confirmCloseOrchestration` — an orchestration card's Close, for the
   *   card an `orchestration_ref` resolved to (`target.orchestrationAgentId`):
   *   the same confirmation, naming every role it will stop.
   *
   * Each answers `undefined` when it opened the confirmation, or the sentence
   * saying why it did not. The table's own `confirm` column PRD #802
   * anticipates is not built; this frontend gate is what satisfies D5 today.
   */
  confirmStopAgent: (target: VoiceDispatchTarget) => string | undefined;
  confirmCloseOrchestration: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * PRD #2119 — an agent's Generate config: OPEN the neutral Yes/No
   * confirmation for the agent an `agent_ref` resolved to (the dashboard row's
   * control), or for the open pane's agent (the pane's control). Like the two
   * stops it only asks: the request is sent by the confirmation's Yes, pressed
   * by the user, bound to the agent as it was when the question opened.
   * Answers `undefined` when it opened the confirmation, or the sentence
   * saying why it did not.
   */
  confirmGenerateConfig: (target: VoiceDispatchTarget) => string | undefined;
  /**
   * Issue #2148 — Retry or Cancel the deck messages waiting on the user for
   * the agent an `agent_ref` resolved to, as the Daemons screen's tile lists
   * them: OPEN a confirmation listing every message the verb covers, by
   * sender and subject. Like the stops it only asks; the confirmation's
   * button sends each Retry or Cancel, bound to the message revisions shown.
   * Served by the deck screen, whose tiles draw the messages. Answers
   * `undefined` when it opened the confirmation, or the sentence saying why
   * it did not.
   */
  confirmInboxAction: (verb: InboxActionVerb, target: VoiceDispatchTarget) => string | undefined;
  /**
   * PRD #1401 — open GitHub's page for one agent's pull request in the app's
   * own browser, over the screen. Answers `undefined` when it opened, or the
   * sentence saying why not (the agent has no pull request, or none the app
   * can open). The badge's click and the `open_pr` row.
   *
   * **Served by the SHELL**, which owns the browser: it opens over the deck's
   * pane and the overview's alike.
   */
  openPullRequest: (target: { deckId: string; agentId: string }) => string | undefined;
  /**
   * Issue #2418 — open the in-app browser on a pull request or issue a card's
   * agent reported, at the address the deck checked. Served by the shell,
   * which owns the browser; the overview reaches it through
   * `OpenCardReference`, behind `openCardReference`.
   */
  openCheckedReference: (target: CardReferenceTarget) => string | undefined;
  /**
   * PRD #1401 — hand the page the browser is showing to the system browser,
   * and close the in-app one once the system browser has it: the toolbar's
   * Open in browser and the `open_pr_in_browser` row. Answers the sentence when
   * no pull request is open, and otherwise the hand-off in flight, which
   * settles to `undefined` when it happened or to the sentence saying why
   * not — in which case the in-app page stays open and says so too.
   */
  openPullRequestInBrowser: () => string | Promise<string | undefined>;
  /**
   * PRD #1401 — close the in-app browser, back to exactly the screen under it.
   *
   * **Published only while the browser is open and nothing of the app's is
   * over it**, for {@link dismissVoiceOverlay}'s reason: {@link closeTopmost}
   * reads its presence, and the browser is at the TOP of that order.
   *
   * Answers the close in flight, which settles to `undefined` once the page
   * has closed, or to the sentence saying why it did not — in which case the
   * browser stays open, says so too, and the next close tries again.
   */
  closePullRequest: () => Promise<string | undefined>;
  /** PRD #1401 — the browser toolbar's Back: one step back in the page's history. */
  pullRequestBack: () => void;
  /*
   * PRD #2374 — the agent dashboard's cards, served by the OVERVIEW. Each
   * answers `undefined` when it happened, or the sentence saying why not; none
   * computes anything about a card — they show, move between and act on what
   * the daemon sent.
   */
  /** Show the dashboard as its table or as cards (remembered per window). */
  setOverviewMode: (mode: OverviewMode) => string | undefined;
  /**
   * PR #2491 decision 2026-11-26 — open one agent's pane from its card: on
   * Terminal (`"terminal"`, a click on the card or the table's Show card), on
   * its Card tab (`"card"`, voice's "show the coder's card"), or on its Card
   * tab at one part of it (a card's `+N`).
   */
  showAgentCard: (target: AgentCardRef, opensOn: PaneOpening) => string | undefined;
  /** Open the pane of the next or previous agent in the dashboard's order, or of the next one that needs the user. */
  stepAgentCard: (step: CardStep) => string | undefined;
  /** Show only the cards that need the user, or every card. */
  setNeedsYouFilter: (on: boolean) => string | undefined;
  /**
   * PR #2491 decision 2026-11-26 — show the open agent pane's Card tab or its
   * Terminal tab. Remembered for that opening only: the next opening of any
   * agent starts on Terminal. Served by the shell while an agent's pane is
   * open; switching to Terminal with no card showing says so and does nothing.
   */
  choosePaneTab: (tab: "card" | "terminal") => string | undefined;
  /** Close the single-action review on screen without sending. Published only while one is open. */
  closeCardReview: () => string | undefined;
  /** Issue #2418 — open or close the tree under one card: the cards the daemon hung under it. Shows the Cards view. */
  setCardTree: (target: AgentCardRef, open: boolean) => string | undefined;
  /**
   * Issue #2418 — open a pull request or issue one card's agent reported, as
   * the deck checked it, in the app's GitHub browser: the one at `url` when
   * given (the badge pressed), else the card's first checked one of `kind`.
   */
  openCardReference: (target: AgentCardRef, kind: "pull_request" | "issue", url?: string) => string | undefined;
  /**
   * Press one action of one card: by id from a button, by its label from
   * voice. A prompt action is sent; one that asks for confirmation, a close,
   * and one whose text the agent wrote open the single-action review instead,
   * and `reply` (the user's own words) opens the agent's pane to type them.
   * `token` is the version of the action the pressed button showed (the
   * daemon refuses a changed one); `review` opens the review showing the exact
   * text instead of sending — what a control that shows only a label asks for
   * (review B8); `openPane` also opens the agent's pane, on Terminal,
   * under the review (the table's row menu).
   */
  runCardAction: (target: AgentCardRef, action: { actionId?: string; label?: string; token?: string; review?: boolean; openPane?: boolean }) => string | undefined;
  /** The single-action review's Send: send the action it is asking about, confirmed. */
  confirmCardAction: () => void;
  /** Open or close one table row's ⋯ menu. */
  toggleOverviewRowMenu: (rowKey: string) => void;
  /*
   * Issue #2399 — one action on many cards. Every batch stops at a
   * confirmation built from the daemon's preview: none of these sends
   * anything but `sendCardBatch`, which is the confirmation's own Send.
   */
  /**
   * Start a batch of one action on a card and every card under it, by the
   * action's id (a button) or its label as spoken: asks the daemon for a
   * preview and opens the confirmation. Sends nothing.
   */
  runCardActionBatch: (target: AgentCardRef, action: { actionId?: string; label?: string }) => string | undefined;
  /** PR #2491 review — empty the Cards view's selection; says so in words when nothing is selected. */
  clearCardSelection: () => string | undefined;
  /**
   * Select or unselect one card; a closed card takes every card under it
   * along. `want` says which (voice's "select" / "unselect"); without it the
   * card flips, as its checkbox does.
   */
  toggleCardSelected: (target: AgentCardRef, want?: boolean) => string | undefined;
  /**
   * Start a batch of one action on the selected cards, by the action's id (a
   * button) or its label as spoken: opens the confirmation. Sends nothing.
   */
  runSelectionBatch: (action: { actionId?: string; label?: string }) => string | undefined;
  /** The batch confirmation's Send: execute the entries the preview included. */
  sendCardBatch: () => void;
  /**
   * Close the batch confirmation, or its results, sending nothing. Refused,
   * with the sentence returned, while the batch is being sent.
   */
  closeCardBatch: () => string | undefined;
  /*
   * PRD #2374 decision 2026-11-24 — the card actions editor. Each answers the
   * sentence saying why it did nothing, or `undefined`. Opening the editor and
   * its forms, reading it again and setting the form's
   * icon and checkboxes are voice's (issue #2399 review 12); every press that
   * writes a daemon's config (Save, a removal's confirmation, Hide, Restore)
   * is the user's own, by hand.
   */
  /**
   * Open the Card actions editor bound to the daemon `deckId` (its ⋯ menu's,
   * or the one voice named); `undefined`: the only daemon showing cards, and
   * with several the sentence asking which (PR #2491 decision 2026-11-25).
   */
  openCardActionsEditor: (deckId?: string) => string | CardActionsWhich | undefined;
  /** Close the editor, or the removal it is asking to confirm. */
  closeCardActionsEditor: () => string | undefined;
  /** Read the shown daemon's card actions again, keeping the form. Writes nothing. */
  refreshCardActions: () => string | undefined;
  /** Open the editor's form for a new action. */
  addCardAction: () => string | undefined;
  /**
   * Open the editor's form for one listed action, by name (a button) or by
   * label as spoken. With the list not read yet (the editor closed, or still
   * reading) it answers a promise instead, settled once the list is read: with
   * `undefined` when the form opened, or the sentence saying why it did not
   * (PR #2491 review, Qodo 3).
   */
  editCardAction: (action: { name?: string; label?: string }) => string | undefined | Promise<string | undefined>;
  /** Ask to remove one of the user's own actions: opens the editor's confirmation. Writes nothing. Answers as {@link editCardAction} does. */
  removeCardAction: (action: { name?: string; label?: string }) => string | undefined | Promise<string | undefined>;
  /** The removal confirmation's Remove: write the removal. */
  confirmRemoveCardAction: () => string | undefined;
  /** Hide a deck default from every card, by name. Writes the daemon's config. */
  hideCardAction: (name: string) => string | undefined;
  /** Bring a deck default back as the deck ships it, by name. Writes the daemon's config. */
  restoreCardAction: (name: string) => string | undefined;
  /** The form's Save: write the action as the form holds it. */
  saveCardAction: () => string | undefined;
  /** The form's Cancel: close it, writing nothing. */
  cancelCardActionForm: () => string | undefined;
  /** Set the form's icon: a grid icon's own name, or an icon's name as spoken. Writes nothing. */
  chooseCardActionIcon: (name: string) => string | undefined;
  /** Tick or untick one of the form's checkboxes — Ask before sending (`confirm`) or Offer on every card (`always`). Writes nothing. */
  setCardActionFlag: (flag: "confirm" | "always", on: boolean) => string | undefined;
  /** Set the form's Placement: Show on the card (`card`, keeping a file's `compact`) or Only in the Card tab (`detail`). Writes nothing. */
  setCardActionPlacement: (placement: "card" | "detail") => string | undefined;
};

/** PRD #2374 — one card, named the way the dashboard's agents are: deck, agent id and the daemon process that listed it. */
export type AgentCardRef = { deckId: string; agentId: string; incarnation?: string };

/**
 * PR #2491 decision 2026-11-26 — where an agent's pane opened from its card
 * starts: on Terminal, on its Card tab, or on its Card tab at one part of it.
 */
export type PaneOpening = "terminal" | "card" | CardTabSection;

/**
 * One registry entry.
 *
 * `run` is deliberately loose here — every entry declares its own precise
 * parameters, and `satisfies` below keeps those signatures at each call site
 * while still proving every entry has the shape the guard and the voice
 * pipeline expect.
 */
export type VoiceActionEntry = {
  /** What the control does, in the app's own words. For humans. */
  label: string;
  /**
   * The {@link VoiceActionContext} members this entry's `run` REQUIRES.
   *
   * It exists because TypeScript's `Pick<VoiceActionContext, …>` on `run` is
   * gone at runtime, and {@link dispatchVoiceAction} has to decide whether the
   * host in front of it can serve this entry BEFORE calling it. Without that
   * decision the only other answer is a `TypeError` — which is what PRD #802 M6
   * shipped, and it reached the user as the words *context.openOverlay is not a
   * function* above the report sentence.
   *
   * An OPTIONAL member is deliberately not listed: `openAgent` reads
   * `selectAgent` through `?.` and works without it, so listing it would refuse
   * a dispatch the entry is happy to serve. {@link NeedsCoversRun} below is the
   * compile-time proof that what IS listed covers what `run` requires, so the
   * dangerous direction — a `run` reading a member nobody declared — is a type
   * error rather than a runtime throw.
   */
  needs: readonly (keyof VoiceActionContext)[];
  run: (...args: never[]) => void;
  /** Set when `commands.toml` carries a row whose `invoke` names this entry. */
  voice?: true;
  /** Why this capability is not a spoken command. Mutually exclusive with `voice`. */
  no_voice?: string;
};

/** PRD #1790 stage 2 — what voice says while a reclaim's dry run is checking, and once it has answered with nothing to read back. */
export const INTERRUPTED_RECLAIM_CHECKING = "Checking what reclaiming it would do…";
const INTERRUPTED_RECLAIM_CHECKED = "Checked.";

/** PRD #1401 — what voice says while the system browser is being handed the page. */
export const PULL_REQUEST_HANDING_OFF = "Opening it in your browser…";
/** …and once it has it: `open_pr_in_browser`'s own `report` in `commands.toml`, said when it is true. */
export const PULL_REQUEST_HANDED_OFF = "Opened it in your browser.";
/** PRD #1401 — what voice says while the pull request page is being closed. */
export const PULL_REQUEST_CLOSING = "Closing the pull request…";
/** …and once it has: `close`'s own `report` in `commands.toml`, said when it is true. */
export const PULL_REQUEST_CLOSED = "Closed.";

/** PRD #2374 — what a card row says when the words named no mode, filter, action or reply it can use. */
const CARD_MODE_UNHEARD = "Say whether you want the table or the cards.";
const CARD_FILTER_UNHEARD = "Say whether you want only the cards that need you, or every card.";
const CARD_ACTION_UNHEARD = "Say which of the card's actions to press, by the label on its button.";
const CARD_BATCH_UNHEARD = "Say which action to send to the card and every card under it, by the label on its button.";
/** What the editor's voice members say when no action name was heard. */
const CARD_ACTION_EDIT_UNHEARD = "Say which card action, by its label in the Card actions editor.";
/** PR #2491 review (Qodo 3): what voice says while a spoken edit or removal waits for the editor to read the list, and once it is done. */
const CARD_ACTIONS_READING = "Reading the card actions…";
export const CARD_ACTION_FORM_OPENED = "Opened the action's form.";
export const CARD_ACTION_REMOVAL_ASKED = "Asking to confirm the removal.";
const CARD_ICON_UNHEARD = "Say which icon, by its name in the Card actions form's icon grid, such as rocket or git merge.";
const CARD_SELECTION_BATCH_UNHEARD = "Say which action to send to the selected cards, by the label on its button.";

/** The card a target names: its deck, agent and, where known, the daemon process that listed it. */
function cardRefOf(target: AgentCardRef): AgentCardRef {
  return { deckId: target.deckId, agentId: target.agentId, ...(target.incarnation !== undefined ? { incarnation: target.incarnation } : {}) };
}

export const VOICE_ACTIONS = {
  // -- the ones the command table names today -----------------------------
  //
  // Deliberately not counted here. A number in a comment is read as a property
  // and this one has been wrong twice; `grep 'voice: true'` is the count.

  openAgent: {
    label: "Open one agent's pane over the current screen",
    voice: true,
    /* `selectAgent` is deliberately not in `needs`: it is read through `?.`, so
       a host without it (the overview, which has no tile to select) serves this
       entry perfectly well and must not be refused for lacking it. */
    needs: ["navigate"],
    /**
     * Opening SELECTS as well, on the deck. That is not decoration: one pane is
     * on screen, so it is the selected one, and it settles the narrow-viewport
     * rule that hides every unselected tile. The overview has no tile to select
     * and passes no `selectAgent`, which is why the member is optional here and
     * required nowhere else.
     */
    run: (context: Pick<VoiceActionContext, "navigate"> & Partial<Pick<VoiceActionContext, "selectAgent" | "revealAgent">>, target: AgentViewTarget) => {
      context.selectAgent?.(target.agentId);
      /* The dashboard's row, scrolled on screen (issue #1492); optional for
         `selectAgent`'s reason — only the overview has rows to reveal. */
      context.revealAgent?.(target.deckId, target.agentId);
      /* The three view members named rather than spread. `VoiceDispatchTarget`
         carries `agentLabel` as well, which belongs to the dictation row and
         not in a `DeckView` — a spread would put it in the app's view state,
         where nothing reads it and everything compares it. */
      context.navigate({ kind: "agent", deckId: target.deckId, agentId: target.agentId, from: target.from, ...("incarnation" in target ? { incarnation: target.incarnation } : {}), ...(target.cardTab ? { cardTab: true } : {}), ...(target.cardSection ? { cardSection: target.cardSection } : {}) });
    },
  },

  dictateToAgent: {
    label: "Type what was said into the open agent's prompt",
    voice: true,
    needs: ["typeIntoAgent"],
    /**
     * **It no longer opens anything, and that is the rebuild in one line.**
     *
     * The old entry navigated to the agent and then aimed the microphone at
     * it, because the row carried an `agent` param and could be said from
     * anywhere. The row is now `screens = ["agent"]`: the target IS the pane on
     * screen, so the visibility requirement — dictated words land in an input
     * the user can see and edit, never a hidden buffer — is satisfied by the
     * precondition rather than by a navigation this entry performs. Rust
     * renders the table's own hint when no pane is open, which is the same
     * sentence any other not-here refusal gets.
     */
    run: (context: Pick<VoiceActionContext, "typeIntoAgent">, target: VoiceDispatchTarget) => context.typeIntoAgent(target),
  },

  submitAgentPrompt: {
    label: "Send what is in the open agent's prompt",
    voice: true,
    needs: ["submitAgentPrompt"],
    /** Presses Enter, and nothing else. It types nothing — a request to write
        something is `dictateToAgent`. */
    run: (context: Pick<VoiceActionContext, "submitAgentPrompt">, target: VoiceDispatchTarget) => context.submitAgentPrompt(target),
  },

  startDictation: {
    label: "Type everything said into the open agent's prompt until told to stop",
    voice: true,
    needs: ["startDictation"],
    /** Targets the pane on screen — the row declares no agent param and is
        `screens = ["agent"]` — and never a pane the user is not looking at. */
    run: (context: Pick<VoiceActionContext, "startDictation">, target: VoiceDispatchTarget) => context.startDictation(target),
  },

  stopDictation: {
    label: "Stop typing what is said into the agent's prompt",
    voice: true,
    needs: ["stopDictation"],
    run: (context: Pick<VoiceActionContext, "stopDictation">) => context.stopDictation(),
  },

  interruptAgent: {
    label: "Interrupt the open agent's turn",
    voice: true,
    needs: ["interruptAgent"],
    /** Presses the deck's interrupt key for the agent — never `Ctrl+C` — and
        only while the agent is working. */
    run: (context: Pick<VoiceActionContext, "interruptAgent">, target: VoiceDispatchTarget) => context.interruptAgent(target),
  },

  clearAgentPrompt: {
    label: "Clear the open agent's prompt",
    voice: true,
    needs: ["clearAgentPrompt"],
    run: (context: Pick<VoiceActionContext, "clearAgentPrompt">, target: VoiceDispatchTarget) => context.clearAgentPrompt(target),
  },

  scratchLastDictation: {
    label: "Remove the words last typed by voice from the open agent's prompt",
    voice: true,
    needs: ["scratchLastDictation"],
    run: (context: Pick<VoiceActionContext, "scratchLastDictation">, target: VoiceDispatchTarget) => context.scratchLastDictation(target),
  },

  startReading: {
    label: "Turn reading on: read every agent's turns aloud",
    voice: true,
    needs: ["startReading"],
    /** No target: reading is the Settings switch, deck-wide, on every screen
        (PRD #1497 decisions 1–3 of 2026-10-09). */
    run: (context: Pick<VoiceActionContext, "startReading">) => context.startReading(),
  },

  stopReading: {
    label: "Turn reading off",
    voice: true,
    needs: ["stopReading"],
    run: (context: Pick<VoiceActionContext, "stopReading">) => context.stopReading(),
  },

  quietSpeech: {
    label: "Silence what the app is saying aloud",
    voice: true,
    needs: ["quietSpeech"],
    run: (context: Pick<VoiceActionContext, "quietSpeech">) => context.quietSpeech(),
  },

  closeTopmost: {
    label: "Close whatever is open over the screen",
    voice: true,
    needs: ["closeAgentView", "reportNothingToClose", "reportRefused"],
    /**
     * PRD #802 — the precedence, decided HERE because it cannot be decided in
     * the table.
     *
     * `screens` draws on `DeckView`, and the voice surface's overlay is a
     * `useState` boolean that is not in it. So the row is callable everywhere
     * and the ordering lives at dispatch: the overlay if it is up, otherwise
     * the New agent dialog if it is open (PRD #1223 U5), otherwise the agent's
     * pane, otherwise an honest report that there was nothing to close. That
     * order is the only one that cannot surprise — the overlay is literally on
     * top of everything, so closing what is underneath it would leave the
     * thing the user was looking at still on screen.
     *
     * **The pull request browser is checked FIRST** (PRD #1401). It is the top
     * of the order whenever it is published: the shell publishes
     * `closePullRequest` only while nothing of the app's is drawn over the
     * browser, so when the voice overlay or a dialog IS over it, that member is
     * absent and the thing on top closes first, as it should.
     *
     * The dialog may REFUSE: while a start is in flight it cannot be closed by
     * any route (PRD #1223 audit F5), and `close` says so in the dialog's own
     * sentence rather than falling through to the pane or answering "nothing
     * to close", which was the lie this branch replaced.
     *
     * `dismissVoiceOverlay` is read through its own presence rather than
     * declared in `needs`: the surface publishes it only while the overlay is
     * open, so its absence IS the answer to "is anything on top?". Declaring it
     * would refuse the whole row whenever the overlay was closed. The same
     * holds for `closeNewAgent`, which the overview publishes only while the
     * dialog is open, and for `closeSettings` and `closeOverlay`, which the
     * shell publishes only while what they close is open and on top.
     *
     * **Settings, and the overlays with no member of their own, are published
     * only while they are on top** (issue #1197, PR #1680 review). The sheet is
     * a shell overlay that stays open across a pane opened over it — by voice,
     * or from the rail — and that pane is then what the user is looking at, so
     * `close` takes the pane first and Settings on the next utterance. Which is
     * on top is the fence's stack (`useInertBackground`), ordered as the app
     * paints them: a sheet opened over the pane or the browser is raised above
     * them, and `close` takes it first. So they are checked before the pane,
     * and whichever is published closes what is on top.
     *
     * **The Host popover is checked last of all** (PR #1672). It hangs from a
     * daemon's Host button IN the dashboard (z-index 30 in the dashboard's own
     * region), so it is under every other layer here, and each of them can be
     * open over it: a click closes it, but a spoken "show commands", "new
     * agent" or "open settings", `Ctrl+N`, or a spoken agent row leaves it open
     * beneath the list (z 96), the New agent dialog (100), the agent's pane
     * (90) or the Settings sheet (80). Nothing opens over the dashboard UNDER
     * it, so with any of those up `close` takes that one first, and the
     * popover on the utterance after — and with none of them up it is what the
     * user is looking at, so `close` takes it rather than answering "nothing
     * to close". The overview publishes `closeHostMetrics` only while one is
     * open, as it does `closeNewAgent`, and the shell withholds it while a stop
     * or upgrade confirmation is open, which is modal over the dashboard too
     * and not in this chain.
     *
     * **The dashboard's counters popover sits just above it** (issue #2312
     * follow-up): it hangs from the top bar, so it is under the same layers
     * and over a Host popover. `closeDashboardCounters` is published and
     * withheld the same way as `closeHostMetrics`.
     */
    run: (
      context: Pick<VoiceActionContext, "closeAgentView" | "reportNothingToClose" | "reportRefused"> & Partial<Pick<VoiceActionContext, "dismissVoiceOverlay" | "closeNewAgent" | "closeSettings" | "closeOverlay" | "closePullRequest" | "reportPending" | "closeHostMetrics" | "closeDashboardCounters">>,
      target: VoiceDispatchTarget,
    ) => {
      /* Reported once the page has closed, for `openPullRequestInBrowser`'s
         reason: a close the platform refused leaves the page on screen. */
      if (context.closePullRequest) {
        const closing = context.closePullRequest();
        context.reportPending?.(PULL_REQUEST_CLOSING, closing, PULL_REQUEST_CLOSED);
        return;
      }
      if (context.dismissVoiceOverlay) return context.dismissVoiceOverlay();
      if (context.closeNewAgent) {
        const refused = context.closeNewAgent();
        if (refused !== undefined) context.reportRefused(refused);
        return;
      }
      if (context.closeSettings) return context.closeSettings();
      if (context.closeOverlay) {
        const refused = context.closeOverlay();
        if (typeof refused === "string") context.reportRefused(refused);
        return;
      }
      if (target.agentViewOpen) return context.closeAgentView();
      /* The counters popover hangs from the top bar, over the dashboard and
         so over a Host popover in it: it goes first of the two. */
      if (context.closeDashboardCounters) return context.closeDashboardCounters();
      if (context.closeHostMetrics) return context.closeHostMetrics();
      context.reportNothingToClose();
    },
  },

  openOverview: {
    label: "Show the agent dashboard",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "overview" }),
  },

  /* Issue #1496 — the agent dashboard's filter. Both write the window
     session's filter, which the dashboard reads, and open the dashboard, so
     what was asked for is what is on screen wherever it was said. A filter
     replaces the one before it. */
  filterDashboard: {
    label: "Filter the agent dashboard",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">, target?: Pick<VoiceDispatchTarget, "dashboardFilter">) => {
      setDashboardFilter(target?.dashboardFilter ?? clearDashboardFilter());
      context.navigate({ kind: "overview" });
    },
  },

  /* "show everything" means the unfiltered dashboard wherever it is said:
     with no filter set this is exactly `openOverview`. The dashboard's Show
     all button is this entry too. */
  clearDashboardFilter: {
    label: "Show the whole agent dashboard, unfiltered",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => {
      setDashboardFilter(clearDashboardFilter());
      context.navigate({ kind: "overview" });
    },
  },

  /**
   * Voice-reachable, and the one row whose screen issue #1198 hides by
   * default: while the flag is off the crate offers it `callable: false`
   * (`voice::schema::hidden_by_flag`) and `DeckShell` refuses it at dispatch,
   * so the row stays in the table and the door stays shut.
   */
  openDeck: {
    label: "Go back to the Daemons screen",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "deck" }),
  },

  closeAgentView: {
    label: "Close the open agent view",
    no_voice: "the agent tile's own X, and what `closeTopmost` calls once it has decided nothing is on top of the pane — a row naming this one DIRECTLY would close the view out from under an open overlay, which is the precedence `close` exists to get right. It is voice-reachable, through that entry and only through it",
    needs: ["closeAgentView"],
    /** The VIEW, never the pane. The agent keeps running and keeps its terminal. */
    run: (context: Pick<VoiceActionContext, "closeAgentView">) => context.closeAgentView(),
  },

  stopVoice: {
    label: "Turn voice control off",
    voice: true,
    needs: ["stopVoice"],
    /**
     * The Voice button's own action, reached by voice.
     *
     * It releases the microphone and nothing else: no screen moves, no view
     * closes, no agent stops. A row that could only be run by speaking would be
     * a second control surface, and this one is the first surface's button.
     */
    run: (context: Pick<VoiceActionContext, "stopVoice">) => context.stopVoice(),
  },

  showVoiceCommands: {
    label: "List what can be said right now",
    voice: true,
    needs: ["showVoiceCommands"],
    /**
     * Opens the list and runs nothing on it. The overlay is generated from the
     * command table, so this entry has no vocabulary of its own to go stale.
     */
    run: (context: Pick<VoiceActionContext, "showVoiceCommands">) => context.showVoiceCommands(),
  },

  /**
   * PRD #1195 M3 — the Deck selector at the top of the deck and the overview.
   * The menu dispatches here, and so does the `switch_deck` row, whose
   * `deck_ref` the app turns into the selector's token before it arrives
   * (`voice::address_deck_switch`). Choosing the deck already shown is a no-op
   * and not an error.
   *
   * `reportRefused` is read through `?.` rather than declared, for
   * `openAgent`'s reason about `selectAgent`: the menu serves no voice surface
   * and never produces a refusal, since it offers only listed decks.
   */
  switchDeck: {
    label: "Switch which daemon the app is showing",
    voice: true,
    needs: ["switchDeck"],
    run: (context: Pick<VoiceActionContext, "switchDeck"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: Pick<VoiceDispatchTarget, "deckSelection" | "deckIdentity">) => {
      const refused = context.switchDeck(target.deckSelection ?? "", target.deckIdentity);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PRD #2274 — the Daemon selector's Add a daemon… entry, the empty-agents
     note's Add a daemon button and the spoken `add_daemon` are one control by
     three routes. Opening the dialog stores nothing: the user fills the form
     in and confirms it there. */
  addDaemon: {
    label: "Add a daemon",
    voice: true,
    needs: ["openAddDaemon"],
    run: (context: Pick<VoiceActionContext, "openAddDaemon">) => context.openAddDaemon(),
  },

  /* PR #1451 round 3, change 4 — a page turn changes which items of a list
     are showing and nothing else. Voice-only: with voice off nothing pages. */
  nextPage: {
    label: "Show the next page of the list on screen",
    voice: true,
    needs: ["turnPage", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "turnPage" | "reportRefused">) => {
      const refused = context.turnPage(1);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  previousPage: {
    label: "Show the previous page of the list on screen",
    voice: true,
    needs: ["turnPage", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "turnPage" | "reportRefused">) => {
      const refused = context.turnPage(-1);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* Issue #1492 — scrolling the agent dashboard, which shows everything
     whether voice is on or off rather than a page at a time. A scroll changes
     which part of the dashboard is showing and nothing else. The shell routes
     `scrollDashboard` to what is in front: a pull request open in the app,
     else an open agent's pane (issue #1673, `lib/paneScroll.ts`), else the
     dashboard. */
  scrollDown: {
    label: "Scroll down by about a screen: the agent dashboard, an open agent's pane, or a pull request open in the app",
    voice: true,
    needs: ["scrollDashboard", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "scrollDashboard" | "reportRefused">) => {
      const refused = context.scrollDashboard("down");
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  scrollUp: {
    label: "Scroll up by about a screen: the agent dashboard, an open agent's pane, or a pull request open in the app",
    voice: true,
    needs: ["scrollDashboard", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "scrollDashboard" | "reportRefused">) => {
      const refused = context.scrollDashboard("up");
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  scrollToTop: {
    label: "Scroll to the top: the agent dashboard, a pull request open in the app, or the history an open agent's pane keeps in the app",
    voice: true,
    needs: ["scrollDashboard", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "scrollDashboard" | "reportRefused">) => {
      const refused = context.scrollDashboard("top");
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  scrollToBottom: {
    label: "Scroll to the bottom: the agent dashboard, a pull request open in the app, or the history an open agent's pane keeps in the app",
    voice: true,
    needs: ["scrollDashboard", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "scrollDashboard" | "reportRefused">) => {
      const refused = context.scrollDashboard("bottom");
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  // -- the rest of the rail and the palette -------------------------------

  openProjects: {
    label: "Manage projects",
    no_voice: "hidden unless the experimental flag is on (issue #1198), so by default a row would be a spoken door to a panel the app does not show; and with the flag on, it opens a picker over project paths the daemon supplies, and no resolver kind in the table can name one of them — a row could open the panel and then leave the user inside a list voice cannot choose from, which is a worse dead end than having no command. It becomes a candidate once a `project_ref` kind exists (PRD #1195 M4)",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("projects"),
  },

  openAgentProfiles: {
    label: "Open agent profiles",
    no_voice: "hidden unless the experimental flag is on (issue #1198), and voice has no per-panel flag gate — `schema::hidden_by_flag` and `DeckShell`'s dispatch gate know only `open_deck`'s — so a row today would be a spoken door to a panel the flag is meant to hide. That gate is the whole reason: opening the form changes nothing until the user edits and saves a profile, so this gets a row when the panel graduates from the flag or the gate is generalised to a per-row feature (PRD #1195 M4, deferred D3)",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("profiles"),
  },

  openOrchestrationOrder: {
    label: "Edit orchestration order",
    no_voice: "hidden unless the experimental flag is on (issue #1198), and voice has no per-panel flag gate — `schema::hidden_by_flag` and `DeckShell`'s dispatch gate know only `open_deck`'s — so a row today would be a spoken door to a panel the flag is meant to hide. That gate is the whole reason: opening the editor launches nothing, since only its own Activate orchestration starts one, so this gets a row on the same terms as `openAgentProfiles`; activating by voice would be a separate row, weighed against PRD #802 D5 (PRD #1195 M4, deferred D3)",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("orchestration"),
  },

  openSettings: {
    label: "Open settings",
    voice: true,
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("settings"),
  },

  showRuns: {
    label: "Show the running agents",
    no_voice: "clears whichever overlays happen to be open and reveals the Daemons screen underneath, so with nothing open it does nothing at all and no honest report sentence can be written for it; `open_deck` is the row that means \"show me the terminals\"",
    needs: ["closeOverlays"],
    run: (context: Pick<VoiceActionContext, "closeOverlays">) => context.closeOverlays(),
  },

  toggleEvidenceDrawer: {
    label: "Show or hide the events drawer",
    no_voice: "a toggle, and the table cannot see which way it is pointing — the drawer is a `ControlDeck` boolean rather than a screen — so \"show the events\" and \"hide the events\" would both flip it and one of the two would be wrong every time",
    needs: ["toggleEvidence"],
    /** No target flips it: the header's Events button and the palette entry.
        `{ open: true }` is an evidence row, which shows the drawer on the item
        it has just selected and never hides it. */
    run: (context: Pick<VoiceActionContext, "toggleEvidence">, target?: { open?: boolean }) => context.toggleEvidence(target?.open),
  },

  focusAgent: {
    label: "Select one agent's tile",
    no_voice: "selects a tile without enlarging it, which is a pointer and keyboard affordance rather than something a supervisor says out loud; `open_agent` is the spoken form of \"show me that one\", and offering both would make every such utterance ambiguous for the model",
    needs: ["selectAgent"],
    /**
     * ONE entry taking the agent as a parameter, exactly as `openAgent` does,
     * rather than one entry per live agent. The palette renders N items and all
     * of them dispatch through here — which is what leaves the guard a fixed
     * key to check, since per-agent entries do not exist at compile time.
     */
    run: (context: Pick<VoiceActionContext, "selectAgent">, target: AgentTarget) => context.selectAgent(target.agentId),
  },

  messageCoordinator: {
    label: "Put the caret in the orchestrator's terminal",
    no_voice: "moves the caret so the operator can type to the orchestration's start role; dictating INTO an agent is PRD #802 D6, and a command that only moved the caret would promise an input path voice cannot finish",
    needs: ["focusTerminal"],
    run: (context: Pick<VoiceActionContext, "focusTerminal">, target: AgentTarget) => context.focusTerminal(target.agentId),
  },

  advanceFixture: {
    label: "Advance the fixture loop one node",
    no_voice: "a fixture-mode debug affordance: it exists only while the app is driven by `FixtureDeckBridge` and moves a deterministic demo loop, so it is not a capability of a shipped build and has no user to speak to",
    needs: ["advanceFixture"],
    run: (context: Pick<VoiceActionContext, "advanceFixture">) => context.advanceFixture(),
  },

  // -- the overview's own -------------------------------------------------

  openNewAgent: {
    label: "Create a new agent on a chosen daemon",
    /* The `open_new_agent` row (PRD #1223). It OPENS the dialog and starts
       nothing — the dialog's own Start is still the only thing that does — so
       it is outside PRD #802 D5's confirmation set. A spoken deck arrives as
       the row's `deck_ref` param, resolved Rust-side against the observed
       fleet, and `App.tsx` carries its value in as `deckId`. */
    voice: true,
    needs: ["openNewAgent"],
    /**
     * The top bar's New agent button, the keyboard shortcut and the first-run
     * note open it with no deck; a deck group's header passes its own, which
     * the deck step preselects, and so does a spoken "new agent on <deck>"
     * (the `open_new_agent` row's `deck_ref`). An empty id preselects nothing.
     */
    run: (context: Pick<VoiceActionContext, "openNewAgent">, target?: { preselectDeckId?: string }) => context.openNewAgent(target?.preselectDeckId || undefined),
  },

  /* PR #1672 — a daemon's Host button and the `show_host_metrics` row. It
     opens a read-only popover and starts, stops and changes nothing. A spoken
     daemon arrives as the row's optional `deck_ref`, which `App.tsx` carries
     in as `preselectDeckId`; the button passes its own. `reportRefused` is
     read through `?.` for `switchDeck`'s reason: the button serves no voice
     surface, and offers only a daemon that has figures to show. */
  showHostMetrics: {
    label: "Show a daemon's Host figures",
    voice: true,
    needs: ["showHostMetrics"],
    run: (context: Pick<VoiceActionContext, "showHostMetrics"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { preselectDeckId?: string }) => {
      const refused = context.showHostMetrics(target?.preselectDeckId || undefined);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2312 follow-up — the dashboard's counters button, by voice. The
     button's click opens through this entry too; `close` closes it. */
  showDashboardCounters: {
    label: "Show the agent dashboard's counters",
    voice: true,
    needs: ["showDashboardCounters"],
    run: (context: Pick<VoiceActionContext, "showDashboardCounters">) => context.showDashboardCounters(),
  },

  /* PRD #1790 stage 2 — a daemon's interrupted work: the banner's Show and
     Hide, and the "Interrupted work" dialog's controls. Every button, key and
     the restart notice's Show dispatch through these. Reclaim and dismiss
     name their item by what the user said (`target.filterText`: an ordinal,
     "the second one", or the item's name), or act on the selected row. */
  showInterruptedWork: {
    label: "Show a daemon's interrupted work",
    voice: true,
    needs: ["showInterruptedWork"],
    run: (context: Pick<VoiceActionContext, "showInterruptedWork"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { preselectDeckId?: string }) => {
      const refused = context.showInterruptedWork(target?.preselectDeckId || undefined);
      /* With several daemons the dashboard asks each again first (final reviewer 2), and reports once they answer. */
      if (refused instanceof Promise) void refused.then((later) => { if (later !== undefined) context.reportRefused?.(later); });
      else if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  hideInterruptedWork: {
    label: "Hide a daemon's interrupted work banner",
    no_voice: "hides a notice for as long as this daemon process runs and records nothing, so a spoken hide would only remove the words that say there is interrupted work to look at; the person can leave the banner, or dismiss the items themselves, which voice reaches (`dismiss_interrupted`, `dismiss_all_interrupted`)",
    needs: ["hideInterruptedWork"],
    run: (context: Pick<VoiceActionContext, "hideInterruptedWork">, deckId: string) => context.hideInterruptedWork(deckId),
  },

  reclaimInterrupted: {
    label: "Check what reclaiming an interrupted item would do, and ask to confirm it",
    voice: true,
    needs: ["interruptedDialog"],
    /* Starts the dry run only: the report reads back its verdict, and the
       reclaim itself waits for the person's click on the confirm. */
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused" | "reportPending">>, target?: { filterText?: string }) => {
      const answer = context.interruptedDialog.reclaim(target?.filterText);
      if (typeof answer === "string") {
        context.reportRefused?.(answer);
        return;
      }
      context.reportPending?.(INTERRUPTED_RECLAIM_CHECKING, answer, INTERRUPTED_RECLAIM_CHECKED);
    },
  },

  confirmReclaimInterrupted: {
    label: "Reclaim the interrupted item the dialog asks about",
    no_voice: "the confirm of a reclaim, which removes a worktree; voice has no confirm-before-acting step yet (PRD #802 D5), so `reclaim_interrupted` runs the dry run, reads its verdict back and stops — the press that removes the worktree is the person's own, by hand",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.confirmReclaim(); },
  },

  /* Voice never completes a dismissal (PRD #802 D5; desktop review R6/A1):
     this asks whether to dismiss the item the words name, bound to its id,
     and the dismissal waits for the person's click or `y` on the question. */
  dismissInterrupted: {
    label: "Ask to dismiss one interrupted item",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { filterText?: string }) => {
      const refused = context.interruptedDialog.askDismiss(target?.filterText);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  confirmDismissInterrupted: {
    label: "Dismiss the interrupted item the question names",
    no_voice: "the answer to a spoken dismissal's question; voice has no confirm-before-acting step yet (PRD #802 D5), so `dismiss_interrupted` asks the question on screen, naming the item, and stops — the click or `y` that dismisses it is the person's own",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.confirmDismiss(); },
  },

  dismissSelectedInterrupted: {
    label: "Dismiss the selected interrupted item",
    no_voice: "the Dismiss button and the `d` key, which dismiss the selected row at once, as the TUI's `d` does; a spoken dismissal must not complete without the person's confirm, so voice reaches it through `dismiss_interrupted`, which asks first",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { void context.interruptedDialog.dismiss(); },
  },

  dismissAllInterrupted: {
    label: "Ask to dismiss every listed interrupted item",
    voice: true,
    needs: ["interruptedDialog"],
    /* Asks the question only; dismissing waits for the person's click. */
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.interruptedDialog.dismissAll();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  confirmDismissAllInterrupted: {
    label: "Dismiss every interrupted item the question named",
    no_voice: "the answer to Dismiss all's question; voice has no confirm-before-acting step yet (PRD #802 D5), so `dismiss_all_interrupted` asks the question on screen and stops — the press that dismisses them is the person's own, by hand",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.confirmDismissAll(); },
  },

  selectInterruptedItem: {
    label: "Select an interrupted item in the list",
    no_voice: "picks the row the buttons act on, a pointer and keyboard affordance; the spoken rows name their item themselves — \"reclaim the second one\", \"dismiss fix-login\" — so voice never needs a selection first",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">, id: string) => context.interruptedDialog.select(id),
  },

  /* Reviewer 11 (CLAUDE.md rule 23): an item's details, its reports and their
     pages by voice. They are shown to read on screen; nothing is read aloud. */
  showInterruptedDetails: {
    label: "Show an interrupted item's details",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { filterText?: string; itemId?: string }) => {
      const refused = context.interruptedDialog.openDetails({ ...(target?.filterText !== undefined ? { item: target.filterText } : {}), ...(target?.itemId !== undefined ? { id: target.itemId } : {}) });
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  showInterruptedReports: {
    label: "Show the first page of an interrupted item's kept reports",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { filterText?: string }) => {
      const refused = context.interruptedDialog.showReports(target?.filterText);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  nextInterruptedReports: {
    label: "Show the next page of an interrupted item's kept reports",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.interruptedDialog.nextReports();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  previousInterruptedReports: {
    label: "Show the previous page of an interrupted item's kept reports",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.interruptedDialog.previousReports();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PRD #1790 stage 3 — Restore. Voice never starts an agent (PRD #802 D5):
     this asks the restore question about the item the words name, and the
     agent starts on the person's own click or key on it. The Restore button
     and `s` open the same question through this entry; for an item that
     lists no agent the dialog asks nothing and says so. */
  askRestoreInterrupted: {
    label: "Ask to restore an interrupted agent",
    voice: true,
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: { filterText?: string }) => {
      const refused = context.interruptedDialog.askRestore(target?.filterText);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  restoreAndContinueInterrupted: {
    label: "Restore the agent the question names, and let it continue",
    no_voice: "Restore and continue starts an agent that spends credits and may push or send; voice has no confirm-before-acting step yet (PRD #802 D5), so `restore_interrupted` asks the question on screen and stops — the click or `c` that starts the agent is the person's own",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.restoreAndContinue(); },
  },

  restoreOnlyInterrupted: {
    label: "Restore the agent the question names, waiting",
    no_voice: "Restore only starts an agent that spends credits and may push or send once told to continue; voice has no confirm-before-acting step yet (PRD #802 D5), so `restore_interrupted` asks the question on screen and stops — the click or `o` that starts the agent is the person's own",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.restoreOnly(); },
  },

  startOverInterrupted: {
    label: "Start the agent the question names over in its worktree",
    no_voice: "Start over starts an agent that spends credits and may push or send, with its saved task; voice has no confirm-before-acting step yet (PRD #802 D5), so `restore_interrupted` asks the question on screen and stops — the click or `w` that starts the agent is the person's own",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => { context.interruptedDialog.startOver(); },
  },

  backInterruptedWork: {
    label: "Back to the interrupted work list",
    no_voice: "backs out of an item's details or a confirmation to the list; voice's `close` does the same while the dialog is the topmost overlay, which is what the dialog's Escape does",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => context.interruptedDialog.back(),
  },

  closeInterruptedWork: {
    label: "Close the Interrupted work dialog",
    no_voice: "the dialog's X and a press outside it; voice's `close` reaches the dialog while it is the topmost overlay, through the fence it registers, so a row naming it directly would close it out from under anything opened over it",
    needs: ["interruptedDialog"],
    run: (context: Pick<VoiceActionContext, "interruptedDialog">) => context.interruptedDialog.close(),
  },

  /* PRD #2374 — the dashboard's agent cards. Every control of the cards view
     dispatches through one of these; none sends anything unasked: a prompt
     action sends exactly the prompt the card shows, one that asks for
     confirmation (and both closes) only opens the card at its confirm step,
     because voice has no confirm step of its own (PRD #802 D5). `reportRefused` is read through `?.` for
     `showHostMetrics`' reason: a button serves no voice surface. */
  setOverviewMode: {
    label: "Show the agent dashboard as a table or as cards",
    voice: true,
    needs: ["setOverviewMode"],
    run: (context: Pick<VoiceActionContext, "setOverviewMode"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: Pick<VoiceDispatchTarget, "overviewMode">) => {
      const refused = target?.overviewMode ? context.setOverviewMode(target.overviewMode) : CARD_MODE_UNHEARD;
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  showAgentCard: {
    label: "Open one agent's pane from its card",
    voice: true,
    needs: ["showAgentCard"],
    /* PR #2491 decision 2026-11-26 — a spoken "show <agent>'s card" asks for
       the card itself, so with no `opensOn` (the voice row sends none) the
       pane opens on its Card tab. A click on a card and the table's Show card
       pass `"terminal"`, as every way into an agent opens on Terminal; a
       card's `+N` names the part of the Card tab it counted. */
    run: (context: Pick<VoiceActionContext, "showAgentCard"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef & { opensOn?: PaneOpening }) => {
      const refused = context.showAgentCard(cardRefOf(target), target.opensOn ?? "card");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  nextAgentCard: {
    label: "Open the next agent's pane, in the dashboard's order",
    voice: true,
    needs: ["stepAgentCard"],
    run: (context: Pick<VoiceActionContext, "stepAgentCard"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.stepAgentCard("next");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  previousAgentCard: {
    label: "Open the previous agent's pane, in the dashboard's order",
    voice: true,
    needs: ["stepAgentCard"],
    run: (context: Pick<VoiceActionContext, "stepAgentCard"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.stepAgentCard("previous");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  nextNeedsYouCard: {
    label: "Open the pane of the next agent that needs you",
    voice: true,
    needs: ["stepAgentCard"],
    run: (context: Pick<VoiceActionContext, "stepAgentCard"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.stepAgentCard("needs-you");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  toggleNeedsYouFilter: {
    label: "Show only the agents that need you, or every agent, in either view",
    voice: true,
    needs: ["setNeedsYouFilter"],
    run: (context: Pick<VoiceActionContext, "setNeedsYouFilter"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target?: Pick<VoiceDispatchTarget, "needsYouOnly">) => {
      const refused = target?.needsYouOnly === undefined ? CARD_FILTER_UNHEARD : context.setNeedsYouFilter(target.needsYouOnly);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PR #2491 decision 2026-11-26 — the agent pane's Card and Terminal tabs,
     switched by voice (the tab strip itself is the pane's own control). */
  showCardTab: {
    label: "Show the Card tab in the open agent pane",
    voice: true,
    needs: ["choosePaneTab"],
    run: (context: Pick<VoiceActionContext, "choosePaneTab"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.choosePaneTab("card");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  showTerminalTab: {
    label: "Leave the Card tab for the Terminal tab in the open agent pane",
    voice: true,
    needs: ["choosePaneTab"],
    run: (context: Pick<VoiceActionContext, "choosePaneTab"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.choosePaneTab("terminal");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2418 — the Cards tree: a card's toggle, and its checked pull
     request and issue badges. */
  openCardTree: {
    label: "Open the tree of cards under one card",
    voice: true,
    needs: ["setCardTree"],
    run: (context: Pick<VoiceActionContext, "setCardTree"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef) => {
      const refused = context.setCardTree(cardRefOf(target), true);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  closeCardTree: {
    label: "Close the tree of cards under one card",
    voice: true,
    needs: ["setCardTree"],
    run: (context: Pick<VoiceActionContext, "setCardTree"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef) => {
      const refused = context.setCardTree(cardRefOf(target), false);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  openCardPullRequest: {
    label: "Open the pull request a card's agent reported, as the deck checked it",
    voice: true,
    needs: ["openCardReference"],
    run: (context: Pick<VoiceActionContext, "openCardReference"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef & { referenceUrl?: string }) => {
      const refused = context.openCardReference(cardRefOf(target), "pull_request", target.referenceUrl);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  openCardIssue: {
    label: "Open the issue a card's agent reported, as the deck checked it",
    voice: true,
    needs: ["openCardReference"],
    run: (context: Pick<VoiceActionContext, "openCardReference"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef & { referenceUrl?: string }) => {
      const refused = context.openCardReference(cardRefOf(target), "issue", target.referenceUrl);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  runCardAction: {
    label: "Press one of a card's actions",
    voice: true,
    needs: ["runCardAction"],
    run: (context: Pick<VoiceActionContext, "runCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef & Pick<VoiceDispatchTarget, "cardActionId" | "cardActionLabel" | "cardActionToken" | "cardActionReview" | "cardActionOpenPane">) => {
      const refused = target.cardActionId === undefined && target.cardActionLabel === undefined
        ? CARD_ACTION_UNHEARD
        : context.runCardAction(cardRefOf(target), {
          ...(target.cardActionId !== undefined ? { actionId: target.cardActionId } : {}),
          ...(target.cardActionLabel !== undefined ? { label: target.cardActionLabel } : {}),
          ...(target.cardActionToken !== undefined ? { token: target.cardActionToken } : {}),
          ...(target.cardActionReview ? { review: true } : {}),
          ...(target.cardActionOpenPane ? { openPane: true } : {}),
        });
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  confirmCardAction: {
    label: "Send the card action the single-action review is asking you to confirm",
    no_voice: "the confirm step of a card action that asks for confirmation, and of both closes; voice has no confirm-before-acting step yet (PRD #802 D5), so `run_card_action` on such an action opens this step on screen and stops — the press that sends it is the user's own, by hand",
    needs: ["confirmCardAction"],
    run: (context: Pick<VoiceActionContext, "confirmCardAction">) => context.confirmCardAction(),
  },

  closeCardReview: {
    label: "Close the single-action review without sending",
    no_voice: "the review's Cancel; voice's `close` reaches the review while it is the topmost overlay, through the fence it registers, as it reaches the batch confirmation, so a row naming it directly would close it out from under anything opened over it",
    needs: ["closeCardReview"],
    run: (context: Pick<VoiceActionContext, "closeCardReview"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.closeCardReview();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2399 — batch card actions. Voice can START a batch on a card and
     everything under it, which only opens the confirmation; sending it is
     the confirmation's Send, by hand (PRD #802 D5). */
  runCardActionBatch: {
    label: "Send one action to a card and every card under it, after confirming",
    voice: true,
    needs: ["runCardActionBatch"],
    run: (context: Pick<VoiceActionContext, "runCardActionBatch"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef & Pick<VoiceDispatchTarget, "cardActionId" | "cardActionLabel">) => {
      const refused = target.cardActionId === undefined && target.cardActionLabel === undefined
        ? CARD_BATCH_UNHEARD
        : context.runCardActionBatch(cardRefOf(target), {
          ...(target.cardActionId !== undefined ? { actionId: target.cardActionId } : {}),
          ...(target.cardActionLabel !== undefined ? { label: target.cardActionLabel } : {}),
        });
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PR #2491 review — every card's checkbox is always shown, so there is no
     selection to turn on; the selection bar's Clear, and voice, empty it. */
  clearCardSelection: {
    label: "Clear the Cards view's selection",
    voice: true,
    needs: ["clearCardSelection"],
    run: (context: Pick<VoiceActionContext, "clearCardSelection"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.clearCardSelection();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  toggleCardSelected: {
    label: "Select or unselect one card for a batch",
    no_voice: "a card's checkbox, which flips; voice says which it wants instead, through `select_agent_card` and `unselect_agent_card` (the same member with `want` set), so saying it twice does not undo it",
    needs: ["toggleCardSelected"],
    run: (context: Pick<VoiceActionContext, "toggleCardSelected"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef) => {
      const refused = context.toggleCardSelected(cardRefOf(target));
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2399 review — voice builds the same selection the checkboxes do,
     one named card at a time, across trees and across the daemons voice
     reaches, and starts a batch on it. Selecting a closed
     card takes everything under it along, as its checkbox does. */
  selectAgentCard: {
    label: "Select one card for a batch",
    voice: true,
    needs: ["toggleCardSelected"],
    run: (context: Pick<VoiceActionContext, "toggleCardSelected"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef) => {
      const refused = context.toggleCardSelected(cardRefOf(target), true);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  unselectAgentCard: {
    label: "Take one card out of the batch selection",
    voice: true,
    needs: ["toggleCardSelected"],
    run: (context: Pick<VoiceActionContext, "toggleCardSelected"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: AgentCardRef) => {
      const refused = context.toggleCardSelected(cardRefOf(target), false);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  runSelectionBatch: {
    label: "Send one action to the selected cards, after confirming",
    voice: true,
    needs: ["runSelectionBatch"],
    run: (context: Pick<VoiceActionContext, "runSelectionBatch"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: Pick<VoiceDispatchTarget, "cardActionId" | "cardActionLabel">) => {
      const refused = target.cardActionId === undefined && target.cardActionLabel === undefined
        ? CARD_SELECTION_BATCH_UNHEARD
        : context.runSelectionBatch({
          ...(target.cardActionId !== undefined ? { actionId: target.cardActionId } : {}),
          ...(target.cardActionLabel !== undefined ? { label: target.cardActionLabel } : {}),
        });
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  sendCardBatch: {
    label: "Send the batch the confirmation is showing",
    no_voice: "the batch confirmation's Send; voice has no confirm-before-acting step yet (PRD #802 D5), so `run_card_action_batch` opens this confirmation on screen and stops — the press that sends the batch is the user's own, by hand",
    needs: ["sendCardBatch"],
    run: (context: Pick<VoiceActionContext, "sendCardBatch">) => context.sendCardBatch(),
  },

  closeCardBatch: {
    label: "Close the batch confirmation or its results",
    no_voice: "the dialog's Cancel, Close and `Escape`; the dialog is on the overlay stack, so voice's `close` (\"cancel\", \"never mind\") closes it as the thing on top, through `closeTopmost` — a row of its own would be a second reading of words `close` already answers. It is voice-reachable through that entry, and refused by every route while the batch is being sent",
    needs: ["closeCardBatch"],
    run: (context: Pick<VoiceActionContext, "closeCardBatch">) => context.closeCardBatch(),
  },

  /* PRD #2374 decision 2026-11-24 — the card actions editor. Voice opens
     the editor, its form for a new action, an action's form and a removal's
     confirmation (the editor of the daemon named), reads it again and sets the form's icon
     and checkboxes (issue #2399 review 12); nothing it says writes the
     daemon's config. Every write —
     Save, the removal's Remove, Hide and Restore — is a press by hand, so a
     misheard word can neither change what an agent is sent nor take an
     action away. */
  /* PR #2491 decision 2026-11-25 — a daemon's ⋯ menu passes its own deck;
     voice, the deck its optional `deck_ref` resolved to (`preselectDeckId`),
     or none, when the only daemon showing cards is meant. */
  openCardActionsEditor: {
    label: "Open one daemon's Card actions editor",
    voice: true,
    needs: ["openCardActionsEditor"],
    run: (context: Pick<VoiceActionContext, "openCardActionsEditor"> & Partial<Pick<VoiceActionContext, "reportRefused" | "askVoiceChoice">>, target?: string | Pick<VoiceDispatchTarget, "preselectDeckId">) => {
      const answer = context.openCardActionsEditor(typeof target === "string" ? target : target?.preselectDeckId || undefined);
      if (answer === undefined) return;
      if (typeof answer === "string") { context.reportRefused?.(answer); return; }
      /* Several daemons and none named: asked which, as a name that fits several agents is. */
      if (context.askVoiceChoice) context.askVoiceChoice({ param: "deck", kind: "deck_ref", candidates: answer.which.map((daemon) => ({ value: daemon.deckId, label: daemon.name })), sentence: answer.refused });
      else context.reportRefused?.(answer.refused);
    },
  },

  closeCardActionsEditor: {
    label: "Close the Card actions editor, or the removal it is asking to confirm",
    no_voice: "the editor's Close and `Escape`; the editor is on the overlay stack, so voice's `close` (\"cancel\", \"never mind\") closes it, or the removal it is asking to confirm, as the thing on top through `closeTopmost` — a row of its own would be a second reading of words `close` already answers",
    needs: ["closeCardActionsEditor"],
    run: (context: Pick<VoiceActionContext, "closeCardActionsEditor">) => { context.closeCardActionsEditor(); },
  },

  refreshCardActions: {
    label: "Read the Card actions editor's list again",
    voice: true,
    needs: ["refreshCardActions"],
    run: (context: Pick<VoiceActionContext, "refreshCardActions"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.refreshCardActions();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  addCardAction: {
    label: "Open the Card actions editor's form for a new action",
    voice: true,
    needs: ["addCardAction"],
    run: (context: Pick<VoiceActionContext, "addCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.addCardAction();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  editCardAction: {
    label: "Open one card action's form in the Card actions editor",
    voice: true,
    needs: ["editCardAction"],
    /* A request that waits for the editor to read its list is reported as it settles: the form opened, or why not. */
    run: (context: Pick<VoiceActionContext, "editCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused" | "reportPending">>, target: Pick<VoiceDispatchTarget, "cardActionId" | "cardActionLabel">) => {
      const refused = target.cardActionId === undefined && target.cardActionLabel === undefined
        ? CARD_ACTION_EDIT_UNHEARD
        : context.editCardAction({
          ...(target.cardActionId !== undefined ? { name: target.cardActionId } : {}),
          ...(target.cardActionLabel !== undefined ? { label: target.cardActionLabel } : {}),
        });
      if (typeof refused === "string") context.reportRefused?.(refused);
      else if (refused !== undefined) context.reportPending?.(CARD_ACTIONS_READING, refused, CARD_ACTION_FORM_OPENED);
    },
  },

  removeCardAction: {
    label: "Ask to remove one of your own card actions",
    voice: true,
    needs: ["removeCardAction"],
    /* A request that waits for the editor to read its list is reported as it settles: the form opened, or why not. */
    run: (context: Pick<VoiceActionContext, "removeCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused" | "reportPending">>, target: Pick<VoiceDispatchTarget, "cardActionId" | "cardActionLabel">) => {
      const refused = target.cardActionId === undefined && target.cardActionLabel === undefined
        ? CARD_ACTION_EDIT_UNHEARD
        : context.removeCardAction({
          ...(target.cardActionId !== undefined ? { name: target.cardActionId } : {}),
          ...(target.cardActionLabel !== undefined ? { label: target.cardActionLabel } : {}),
        });
      if (typeof refused === "string") context.reportRefused?.(refused);
      else if (refused !== undefined) context.reportPending?.(CARD_ACTIONS_READING, refused, CARD_ACTION_REMOVAL_ASKED);
    },
  },

  confirmRemoveCardAction: {
    label: "Remove the card action the editor is asking to confirm",
    no_voice: "the removal confirmation's Remove, which deletes the action from the daemon's config; voice has no confirm-before-acting step yet (PRD #802 D5), so `remove_card_action` opens this confirmation on screen and stops — the press that removes it is the user's own, by hand",
    needs: ["confirmRemoveCardAction"],
    run: (context: Pick<VoiceActionContext, "confirmRemoveCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.confirmRemoveCardAction();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  hideCardAction: {
    label: "Hide a deck default card action",
    no_voice: "Hide writes the daemon's config at once, with no step to read first, and takes the action off every card; voice opens the editor and an action's form, and every write in the editor is a press by hand, so a misheard name cannot take an action off the agents' cards",
    needs: ["hideCardAction"],
    run: (context: Pick<VoiceActionContext, "hideCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>, name: string) => {
      const refused = context.hideCardAction(name);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  restoreCardAction: {
    label: "Restore a deck default card action",
    no_voice: "Restore writes the daemon's config at once and throws away the user's own version of that default; voice opens the editor and an action's form, and every write in the editor is a press by hand",
    needs: ["restoreCardAction"],
    run: (context: Pick<VoiceActionContext, "restoreCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>, name: string) => {
      const refused = context.restoreCardAction(name);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  saveCardAction: {
    label: "Save the card action the editor's form holds",
    no_voice: "the form's Save writes the action — the exact text agents are sent — to the daemon's config; voice opens the form (`add_card_action`, `edit_card_action`), the fields are typed by hand, and the press that writes them is the user's own, so a misheard word never becomes text an agent is sent",
    needs: ["saveCardAction"],
    run: (context: Pick<VoiceActionContext, "saveCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.saveCardAction();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  chooseCardActionIcon: {
    label: "Choose the icon of the card action in the editor's form",
    voice: true,
    needs: ["chooseCardActionIcon"],
    /* The grid passes the icon it draws; voice, the icon's name as the user said it (`filterText`). */
    run: (context: Pick<VoiceActionContext, "chooseCardActionIcon"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: string | Pick<VoiceDispatchTarget, "filterText">) => {
      const said = typeof target === "string" ? target : target.filterText;
      const refused = said ? context.chooseCardActionIcon(said) : CARD_ICON_UNHEARD;
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* The form's two checkboxes, one entry per state each, so a spoken "ask
     before sending" never toggles a box that was already ticked. The boxes
     dispatch the entry for the state they are being set to. */
  askBeforeSendingCardAction: {
    label: "Tick Ask before sending in the Card actions form",
    voice: true,
    needs: ["setCardActionFlag"],
    run: (context: Pick<VoiceActionContext, "setCardActionFlag"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionFlag("confirm", true);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  sendCardActionWithoutAsking: {
    label: "Untick Ask before sending in the Card actions form",
    voice: true,
    needs: ["setCardActionFlag"],
    run: (context: Pick<VoiceActionContext, "setCardActionFlag"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionFlag("confirm", false);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  offerCardActionOnEveryCard: {
    label: "Tick Offer on every card in the Card actions form",
    voice: true,
    needs: ["setCardActionFlag"],
    run: (context: Pick<VoiceActionContext, "setCardActionFlag"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionFlag("always", true);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  offerCardActionWhenAgentsChoose: {
    label: "Untick Offer on every card in the Card actions form",
    voice: true,
    needs: ["setCardActionFlag"],
    run: (context: Pick<VoiceActionContext, "setCardActionFlag"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionFlag("always", false);
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2399 re-review N4: the form's Placement choice, one entry per
     choice, so saying one twice keeps it. The select dispatches the entry
     for the choice picked. */
  showCardActionOnCard: {
    label: "Choose Show on the card as the Card actions form's Placement",
    voice: true,
    needs: ["setCardActionPlacement"],
    run: (context: Pick<VoiceActionContext, "setCardActionPlacement"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionPlacement("card");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  showCardActionOnlyInCardTab: {
    label: "Choose Only in the Card tab as the Card actions form's Placement",
    voice: true,
    needs: ["setCardActionPlacement"],
    run: (context: Pick<VoiceActionContext, "setCardActionPlacement"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.setCardActionPlacement("detail");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  cancelCardActionForm: {
    label: "Close the Card actions editor's form without saving",
    no_voice: "the form's Cancel, which only closes the form; voice's `close` closes the editor it is in, and saying another action's name (`edit_card_action`, `add_card_action`) replaces the form",
    needs: ["cancelCardActionForm"],
    run: (context: Pick<VoiceActionContext, "cancelCardActionForm">) => { context.cancelCardActionForm(); },
  },

  toggleOverviewRowMenu: {
    label: "Open or close a dashboard row's menu",
    no_voice: "each item it holds is reachable by its own voice command — `show_agent_card` and `run_card_action` — so a spoken door to the menu itself would open a list the user then has to point at",
    needs: ["toggleOverviewRowMenu"],
    run: (context: Pick<VoiceActionContext, "toggleOverviewRowMenu">, rowKey: string) => context.toggleOverviewRowMenu(rowKey),
  },

  /* PRD #1223 — the three `requires`-gated rows: the New agent dialog's
     directory browser. None starts anything — the dialog's Start is still the
     only thing that does — so none is in PRD #802 D5's confirmation set. The
     manual paths (the `..` row, the keys, the Use this directory button) call
     the same dialog functions directly and are unchanged. */
  openDirectory: {
    label: "Open a directory listed in the New agent browser",
    voice: true,
    needs: ["openDirectory", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "openDirectory" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.openDirectory(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  goToParentDirectory: {
    label: "Go up to the parent directory in the New agent browser",
    voice: true,
    needs: ["goToParentDirectory", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "goToParentDirectory" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.goToParentDirectory(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  useThisDirectory: {
    label: "Use the directory on screen for the new agent",
    voice: true,
    needs: ["useThisDirectory", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "useThisDirectory" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.useThisDirectory(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* PR #1451 round 3, change 5 — the browser's Filter box. Narrowing or
     widening the listing starts and chooses nothing, and typing in the box is
     unchanged: it calls the same dialog function. */
  filterDirectories: {
    label: "Filter the New agent browser's directories",
    voice: true,
    needs: ["filterDirectories", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "filterDirectories" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.filterDirectories(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  clearDirectoryFilter: {
    label: "Clear the New agent browser's directory filter",
    voice: true,
    needs: ["clearDirectoryFilter", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "clearDirectoryFilter" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.clearDirectoryFilter(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* PRD #1223 — the `new_agent_form` rows: Mode, Agent and Name, and since
     PR #1451 round 4 (D8) Command. None starts anything, so none is in PRD
     #802 D5's confirmation set; the manual chips and the Name and Command
     inputs are unchanged and call the same functions. */
  chooseNewAgentMode: {
    label: "Choose a Mode chip in the New agent form",
    voice: true,
    needs: ["chooseNewAgentMode", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "chooseNewAgentMode" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.chooseNewAgentMode(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  chooseNewAgentType: {
    label: "Set the New agent form's Command to an agent's default command",
    voice: true,
    needs: ["chooseNewAgentType", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "chooseNewAgentType" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.chooseNewAgentType(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  nameNewAgent: {
    label: "Set the New agent form's Name",
    voice: true,
    needs: ["nameNewAgent", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "nameNewAgent" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.nameNewAgent(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  setNewAgentCommand: {
    label: "Set the New agent form's Command",
    voice: true,
    needs: ["setNewAgentCommand", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "setNewAgentCommand" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.setNewAgentCommand(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* Issue #1263 — the deck field. It starts nothing, so it is outside PRD
     #802 D5's set; the manual path (a click, or Enter on a highlighted row)
     calls the same `chooseDeck`. */
  chooseNewAgentDeck: {
    label: "Choose the daemon in the New agent dialog",
    voice: true,
    needs: ["chooseNewAgentDeck", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "chooseNewAgentDeck" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.chooseNewAgentDeck(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* Issue #1247 — the dialog's Discard. It stops nothing and starts nothing,
     so it is outside D5's set too; what it cannot undo is the form, which is
     why its row needs the whole utterance. */
  discardNewAgent: {
    label: "Discard the New agent form and close it",
    voice: true,
    needs: ["discardNewAgent", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "discardNewAgent" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.discardNewAgent(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* The New agent dialog's Start, by voice (PRD #1223). It calls the function
     the button calls and starts at once — PRD #802 D5's start half was
     revisited on 2026-09-23 — or reports why the form cannot start. */
  startNewAgent: {
    label: "Start the agent the New agent form describes",
    voice: true,
    needs: ["startNewAgent", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "startNewAgent" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.startNewAgent(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* PRD #1885 — the `Review project commands` dialog. Approving starts the
     orchestration with exactly the commands and prompts on screen, as the
     dialog's own button does; it is a start the user is looking at, which is
     why voice may press it (docs/develop/voice-first-design.md section 5). */
  approveProjectCommands: {
    label: "Approve the project commands the review shows and start the orchestration",
    voice: true,
    needs: ["approveProjectCommands"],
    /* `reportRefused` is read through `?.`, for `openPullRequest`'s reason: the
       button is disabled whenever it would be refused, so its click serves no
       voice surface. */
    run: (context: Pick<VoiceActionContext, "approveProjectCommands"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.approveProjectCommands();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* Issue #2511 — the marker saying an approval is waiting while something the
     user is in holds its review back. It opens the review and nothing else:
     approving or denying is still the review's own, so voice may press it for
     the reason it may open any dialog. */
  showApprovalRequest: {
    label: "Open the approval review a deck is waiting on, over whatever is open",
    voice: true,
    needs: ["showApprovalRequest"],
    /* `reportRefused` is read through `?.`, as `approveProjectCommands` does:
       the marker's click always has a review to open. */
    run: (context: Pick<VoiceActionContext, "showApprovalRequest"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.showApprovalRequest();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  cancelProjectCommands: {
    label: "Close the project commands review without starting anything",
    no_voice: "the review dialog's Cancel and its `Escape`; the dialog is on the overlay stack, so voice's `close` (\"cancel\", \"never mind\") closes it as the thing on top, through `closeTopmost` — a row of its own would be a second reading of words `close` already answers. It is voice-reachable through that entry",
    needs: ["cancelProjectCommands"],
    run: (context: Pick<VoiceActionContext, "cancelProjectCommands">) => context.cancelProjectCommands(),
  },

  /* PRD #2174 — the star reminder's three buttons, which voice may press for
     the reason it may press any button on a dialog the user is looking at:
     each acts only while the reminder is showing, and none starts or stops
     anything. `reportRefused` is read through `?.`, as `approveProjectCommands`
     does: a click on the button serves no voice surface. */
  starRepo: {
    label: "Star the repository from the star reminder",
    voice: true,
    needs: ["starRepo"],
    run: (context: Pick<VoiceActionContext, "starRepo"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.starRepo();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  remindStarLater: {
    label: "Ask about starring the repository again later",
    voice: true,
    needs: ["remindStarLater"],
    run: (context: Pick<VoiceActionContext, "remindStarLater"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.remindStarLater();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  dismissStarPrompt: {
    label: "Stop the star reminder from asking again",
    voice: true,
    needs: ["dismissStarPrompt"],
    run: (context: Pick<VoiceActionContext, "dismissStarPrompt"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.dismissStarPrompt();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PRD #211 — the Antigravity notice's buttons, which voice may press for
     the reason it may press the star reminder's: each acts only while the
     notice is showing, and none starts or stops anything. Later has no row of
     its own: it is what voice's `close` does to the notice, as `Esc` is. */
  openAntigravityIssues: {
    label: "Open the issues page from the Antigravity notice",
    voice: true,
    needs: ["openAntigravityIssues"],
    run: (context: Pick<VoiceActionContext, "openAntigravityIssues"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.openAntigravityIssues();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  remindAntigravityLater: {
    label: "Close the Antigravity notice until the next interval",
    no_voice: "the notice's Later and its `Escape`; the notice is on the overlay stack, so voice's `close` (\"cancel\", \"never mind\", \"close it\") closes it as the thing on top, which is Later, through `closeTopmost` — a row of its own would compete with the star reminder's `remind_star_later` for the same word \"later\", for a dialog `close` already answers. It is voice-reachable through that entry",
    needs: ["remindAntigravityLater"],
    run: (context: Pick<VoiceActionContext, "remindAntigravityLater"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.remindAntigravityLater();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  dismissAntigravityNotice: {
    label: "Stop the Antigravity notice from showing again",
    voice: true,
    needs: ["dismissAntigravityNotice"],
    run: (context: Pick<VoiceActionContext, "dismissAntigravityNotice"> & Partial<Pick<VoiceActionContext, "reportRefused">>) => {
      const refused = context.dismissAntigravityNotice();
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  /* PRD #802 D5 — the two rows that STOP something. Each entry calls a member
     that can only OPEN a confirmation (see the members' own comment on
     `VoiceActionContext`), so voice never reaches `runAction` for a stop: the
     confirmation's button does, pressed by hand. The manual controls — a
      dashboard row's Stop and a card's Close — keep their own dispatch seam.
      The agent pane's Close agent uses this registry entry too (issue #1402). */

  confirmStopAgent: {
    label: "Ask to close one agent on the dashboard or in its pane",
    voice: true,
    needs: ["confirmStopAgent", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmStopAgent" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmStopAgent(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* PRD #2119 — Generate config, by voice or by an agent's own control on
     the dashboard row and in its pane. It only OPENS the confirmation; the
     request goes when the user presses Yes. */
  confirmGenerateConfig: {
    label: "Ask to generate the project's config with one agent",
    voice: true,
    needs: ["confirmGenerateConfig", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmGenerateConfig" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmGenerateConfig(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* Issue #2148 — Retry and Cancel on an agent's deck messages that wait on
     the user, by voice. Each only OPENS the confirmation listing them; a
     row's own Retry or Cancel button acts on its one message directly. */
  confirmRetryInboxMessage: {
    label: "Ask to retry the deck messages waiting on one agent",
    voice: true,
    needs: ["confirmInboxAction", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmInboxAction" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmInboxAction("retry", target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  confirmCancelInboxMessage: {
    label: "Ask to cancel the deck messages waiting on one agent",
    voice: true,
    needs: ["confirmInboxAction", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmInboxAction" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmInboxAction("cancel", target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  confirmCloseOrchestration: {
    label: "Ask to close an orchestration on the dashboard",
    voice: true,
    needs: ["confirmCloseOrchestration", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmCloseOrchestration" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmCloseOrchestration(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  /* PRD #1401 — an agent's pull request, in the app's own browser. Opening,
     handing off and closing change what is on screen and nothing else; the
     page itself is GitHub's, and nothing here acts on the pull request. */
  openPullRequest: {
    label: "Open an agent's pull request in the app",
    voice: true,
    /* `reportRefused` is read through `?.`, for `switchDeck`'s reason: the
       badge offers itself only for a pull request the app can open, so its
       click never produces a refusal and serves no voice surface. */
    needs: ["openPullRequest"],
    run: (context: Pick<VoiceActionContext, "openPullRequest"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: Pick<VoiceDispatchTarget, "deckId" | "agentId">) => {
      const refused = context.openPullRequest({ deckId: target.deckId, agentId: target.agentId });
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  openPullRequestInBrowser: {
    label: "Open the pull request on screen in the system browser",
    voice: true,
    needs: ["openPullRequestInBrowser"],
    /* The hand-off is reported when it has happened, not when it was asked
       for: a system browser that would not open leaves the page in the app,
       and "Opened it in your browser" would then be false. */
    run: (context: Pick<VoiceActionContext, "openPullRequestInBrowser"> & Partial<Pick<VoiceActionContext, "reportRefused" | "reportPending">>) => {
      const handoff = context.openPullRequestInBrowser();
      if (typeof handoff === "string") context.reportRefused?.(handoff);
      else context.reportPending?.(PULL_REQUEST_HANDING_OFF, handoff, PULL_REQUEST_HANDED_OFF);
    },
  },

  closePullRequest: {
    label: "Close the pull request browser",
    no_voice: "the browser toolbar's Close and the app's `Escape`, and what `closeTopmost` calls first while the browser is the top of the screen — a row naming this one directly would close the browser out from under the voice overlay or a dialog drawn over it, which is the precedence `close` exists to get right. It is voice-reachable through that entry",
    needs: ["closePullRequest"],
    run: (context: Pick<VoiceActionContext, "closePullRequest">) => context.closePullRequest(),
  },

  pullRequestBack: {
    label: "Go back one page in the pull request browser",
    no_voice: "a step through the page's own history, which the user steers with the mouse and keyboard inside the page; PRD #1401 asks voice for the four scrolls, `close` and the hand-off to the system browser there, and a spoken \"back\" would be one more reading of a word `close` already answers",
    needs: ["pullRequestBack"],
    run: (context: Pick<VoiceActionContext, "pullRequestBack">) => context.pullRequestBack(),
  },
} satisfies Record<string, VoiceActionEntry>;

/** Every action id, as the guard and the command table spell them. */
type VoiceActionId = keyof typeof VOICE_ACTIONS;

/**
 * Compile-time proof that each entry's `needs` covers what its `run` requires.
 *
 * `needs` is read at runtime and `Pick<VoiceActionContext, …>` is not, so the
 * two could disagree — and an UNDER-declared `needs` is the dangerous
 * direction: {@link dispatchVoiceAction} would wave the entry through and the
 * `run` would throw on a member nobody checked, which is the defect PRD #802 M6
 * shipped. This maps each entry's `run` against the signature its own `needs`
 * describes; parameters are contravariant, so a `run` requiring a member the
 * list omits is not assignable and the line below fails to compile, naming the
 * entry.
 *
 * It does NOT refuse an OVER-declared `needs` — listing a member `run` never
 * touches makes dispatch stricter than it has to be, which is the conservative
 * side and is a refusal rather than a throw.
 */
type NeedsCoversRun = {
  [K in VoiceActionId]: (typeof VOICE_ACTIONS)[K]["run"] extends (
    context: Pick<VoiceActionContext, (typeof VOICE_ACTIONS)[K]["needs"][number]>,
    ...rest: never[]
  ) => void
    ? K
    : ["this entry's `run` reads a context member its `needs` does not declare", K];
};
const NEEDS_COVERS_RUN: { [K in VoiceActionId]: K } = null as unknown as NeedsCoversRun;
void NEEDS_COVERS_RUN;

/**
 * Everything a voice dispatch can hand an action, in one object.
 *
 * ONE shape rather than a per-action argument builder, because a per-action
 * builder is a second list of the ids — and the acceptance criterion this whole
 * design exists for is that adding a command changes no implementation: a row in
 * `commands.toml` and the `voice: true` flip in this file, plus the tests that pin
 * the shipped row set by value. (PRD #802 M8 measured it at seven files, five of
 * them test-only. The criterion was written as "a ONE-FILE change" and that count
 * was wrong; the substance — no implementation — held.) An entry reads the
 * members it declares and ignores the rest,
 * which is the same latitude every `Pick<VoiceActionContext, …>` above already
 * takes with the context.
 *
 * It is `AgentViewTarget` because that is the widest target any entry takes;
 * `AgentTarget`'s single member is a subset of it.
 */
/**
 * Whether `transcript` says the word "command" — `voice::table::spoken_words`'
 * reading of it, lowercased runs of letters and digits, and the singular only,
 * as `voice::outcome::COMMAND_PHRASE_OPENER` ("what commands can I say" is not
 * about the field).
 */
export function saysCommand(transcript: string): boolean {
  return transcript.toLowerCase().split(/[^\p{L}\p{N}]+/u).includes("command");
}

export type VoiceDispatchTarget = AgentViewTarget & {
  /**
   * The deck to PRESELECT — what a row's `deck_ref` param resolved to (PRD
   * #1223), and absent when the user named none. For `choose_deck` (#1263)
   * it is the deck to choose in the open dialog's deck field.
   *
   * Deliberately not `deckId` above. That member is the deck an AGENT lives
   * on, and `App.tsx` fills it for every dispatch, falling back to the
   * selected deck — so an entry reading it could not tell "the user said the
   * build box" from "the user said nothing and the build box is selected", and
   * the bare "new agent" would preselect whatever deck happened to be in view.
   */
  preselectDeckId?: string;
  /**
   * The Deck selector token a `switch_deck` row's `deck_ref` resolved to (PRD
   * #1195 M3) — `local` or a configured deck's row id, which the app
   * substitutes for the fleet key Rust-side (`voice::address_deck_switch`),
   * and empty when the deck had none. Its own member rather than
   * {@link preselectDeckId}, which is a fleet key.
   */
  deckSelection?: string;
  /**
   * The address of the remote row {@link deckSelection} named when Rust
   * resolved the switch (PRD #1195) — absent for the local deck, which has
   * none. A row id survives Settings editing any of the row's address fields
   * (`REMOTE_ADDRESS_FIELDS`), so the switch compares this with the row before
   * writing and refuses one that now reaches a different machine or deck, or
   * the same deck by a different route.
   */
  deckIdentity?: VoiceDeckIdentityDto;
  /**
   * The child directory to open — the deck's own path a row's `dir_ref` param
   * resolved to, against the browser's children on screen (PRD #1223).
   */
  directoryPath?: string;
  /**
   * The text for the directory browser's Filter box — what a row's
   * `filter_text` param resolved to (PR #1451 round 3, change 5). The model
   * extracted it ("letter D" is `d`) and Rust accepted it only because the
   * user said it, so an entry reading this member reads the user's own words
   * or nothing. Lowercase; the box matches case-insensitively.
   */
  filterText?: string;
  /**
   * The command line for the New agent form's Command field — what a row's
   * `command_text` param resolved to (PR #1451 round 4, decision D8). Rust
   * kept the model's value only because the transcript holds it, and this is
   * the TRANSCRIPT's slice of it, as said, less surrounding quotes and a
   * sentence's full stop.
   */
  commandText?: string;
  /**
   * The utterance said the word "command" — it was about the Command field
   * (PR #1451 round 4, audit A1). `voiceStart` refuses to start on such a
   * sentence, whatever the backend answered: Start acts on the form as shown,
   * and "Set the command to devbox run agent" (whose `run` is a start word)
   * or "set the command to bash and start it" asks for a different form.
   * Rust's action grounding refuses the same sentences
   * (`voice::outcome::heard_outside_command`); this is the dialog's own half.
   */
  saysCommand?: boolean;
  /**
   * The directory browser the utterance was JUDGED against: the deck and the
   * listing `path` the webview declared with it (PRD #1223), or absent when it
   * declared none.
   *
   * The screen gets `SCREEN_MOVED_ON` in the voice surface for the same
   * hazard, but a browser that moved is not a reason to refuse every command —
   * "close" is still right after a listing lands — so the check lives in the
   * three directory members, which compare this against the live browser and
   * refuse when the two differ.
   */
  declaredDirectories?: { deckId: string; path: string };
  /**
   * The Mode chip a `mode_ref` resolved to — its id in the form as declared
   * (PRD #1223).
   */
  modeId?: string;
  /** The agent entry an `agent_type_ref` resolved to — its registry id. */
  agentTypeId?: string;
  /**
   * The dashboard filter a `filter_dashboard` dispatch asks for, built from
   * its resolved facets (issue #1496).
   */
  dashboardFilter?: DashboardFilter;
  /** PRD #2374 — the dashboard view a `set_overview_mode` dispatch asks for. */
  overviewMode?: OverviewMode;
  /** PRD #2374 — whether a `filter_needs_you_cards` dispatch asks for only the cards that need the user. */
  needsYouOnly?: boolean;
  /** PRD #2374 — the card action a button presses, by the daemon's id. */
  cardActionId?: string;
  /** PRD #2374 — the card action a `run_card_action` dispatch names, by its label as the user said it. */
  cardActionLabel?: string;
  /** PRD #2374 — the version (`token`) of the card action the pressed button showed. */
  cardActionToken?: string;
  /** PRD #2374 — open a review showing the action's exact text instead of sending it (review B8). */
  cardActionReview?: boolean;
  /** PR #2491 decision 2026-11-25 — also open the agent's pane, on Terminal, under the review (the table's row menu). */
  cardActionOpenPane?: boolean;
  /**
   * The New agent form the utterance was JUDGED against — its deck and chosen
   * directory as declared — or absent when no live form was declared. The
   * form members refuse when the live form differs, for
   * {@link declaredDirectories}' reason.
   */
  declaredForm?: { deckId: string; path: string };
  /**
   * The orchestration card an `orchestration_ref` resolved to, named by one
   * of its members' agent ids on {@link deckId} — a member, because a card
   * whose daemon reported no orchestration id is still a card (PRD #1223).
   */
  orchestrationAgentId?: string;
  /**
   * PRD #1666 part 2 (G-fix7) — the daemon process that listed
   * {@link orchestrationAgentId} when the utterance was declared. A close
   * confirms only the card whose member that process still lists; absent
   * where the declaration did not list it.
   */
  orchestrationIncarnation?: string;
  /**
   * The words to type into the open agent's prompt, for the dictation row
   * (PRD #802 D6, rebuilt).
   *
   * **Resolved Rust-side from the TRANSCRIPT, never supplied by the model.**
   * It arrives as the `spoken_prefix` param's `value`, which
   * `voice::dictation::strip_opening` produced by verifying the model's marked
   * boundary against the app's own transcript and slicing what follows it. A
   * boundary that did not verify never becomes a dispatch, so an entry reading
   * this member is reading the user's own words or nothing.
   */
  text?: string;
  /**
   * Press Enter after {@link text} has been typed — the utterance ended with a
   * separate send sentence in typing mode (PR #1451 round 3), which Rust
   * decided and the outcome's `thenSubmit` carries. Read by `typeIntoAgent`
   * alone.
   */
  thenSubmit?: boolean;
  /**
   * Whether an agent's pane is open over the screen.
   *
   * Read by {@link closeTopmost} to decide whether there is anything under the
   * overlay to close. It is a boolean rather than the target's own `agentId`
   * because that id is the one a row's `agent_ref` param resolved to, which for
   * a row with no such param is the empty string.
   */
  agentViewOpen?: boolean;
  /**
   * What the deck CALLS the agent, for a surface that has to name it.
   *
   * Resolved Rust-side against live state and carried on the dispatch outcome's
   * param; `App.tsx` copies it here. Optional because it is meaningful only to
   * a row that declares an `agent_ref` param, and an entry that does not read
   * it is unaffected by its absence.
   *
   * It is the DAEMON's text — a display name a user chose — so a surface that
   * renders it scrubs and bounds it at the render seam, the way every other
   * free-form string in this app is treated.
   */
  agentLabel?: string;
};

/**
 * What a host offers a voice dispatch: `navigate` and `closeAgentView` always,
 * plus whatever else that host happens to have.
 *
 * # The absent members are checked now, and linkage-check rule 14 still does not check them
 *
 * The guard proves an `invoke` names an entry and that the entry is classified.
 * It says nothing about whether the HOST can serve the context that entry's `run`
 * reads — those are different questions, and linkage-check rule 14 answers only the first. What
 * answers the second is {@link VoiceActionEntry.needs}, read by
 * {@link dispatchVoiceAction} before the call: a host that cannot serve an entry
 * gets a refusal it can report, rather than a `TypeError` whose message reaches
 * the user as prose the app never wrote.
 *
 * **`DeckShell` merges the deck's own context in while the deck is mounted**, so
 * the answer is "yes" for every member on the deck and "the shell's two" on the
 * overview — `DeckSurface` publishes upward through a ref, and the shell reads it
 * at dispatch time. That is what makes `open_settings` a table row rather than a
 * plumbing project; it does not make the check redundant, because the overview
 * genuinely cannot open a deck overlay and has to say so.
 */
export type VoiceDispatchContext = Pick<VoiceActionContext, "navigate" | "closeAgentView"> & Partial<VoiceActionContext>;

/**
 * The members the VOICE SURFACE itself serves, and the channel it publishes
 * them through (PRD #802, the `voice_off` row).
 *
 * {@link VoiceContextChannel} below is a screen publishing upward to its
 * parent; this is the same mechanism pointed the other way round the screen
 * switch. The voice surface is a sibling of that switch (see `App.tsx`'s
 * `DeckShell`), so it is mounted on every screen — which is precisely what a
 * command that must never be unavailable needs, and what neither the deck nor
 * the shell can offer.
 *
 * A narrow `Pick` rather than a `Partial<VoiceActionContext>`, so the split is
 * legible at both ends and enforced at one: {@link VoiceScreenContext} is this
 * set's complement, so a screen that tried to serve one of these members would
 * not type-check, and neither would a panel that left one out.
 */
export type VoicePanelContext = Pick<VoiceActionContext, "stopVoice" | "showVoiceCommands" | "typeIntoAgent" | "submitAgentPrompt" | "startDictation" | "stopDictation" | "interruptAgent" | "clearAgentPrompt" | "scratchLastDictation" | "startReading" | "stopReading" | "quietSpeech" | "dismissVoiceOverlay" | "reportNothingToClose" | "reportRefused" | "reportPending" | "askVoiceChoice">;

/**
 * PR #2491 decision 2026-11-25 — what a dispatch the screen could not settle
 * asks the user to choose from: the row's `param` of `kind`, one candidate
 * per thing on screen (its value, as the param's resolver gives it, and the
 * name the screen shows), and the sentence said when no choice is offered.
 */
export interface VoiceChoiceAsk {
  param: string;
  kind: "deck_ref";
  candidates: { value: string; label: string }[];
  sentence: string;
}
/**
 * `Partial`, because a panel can serve one of these and not another.
 *
 * `showVoiceCommands` needs a runtime verb to list anything with, and the voice
 * members of `DeckRuntimeState` are all optional — so a panel in front of a
 * runtime that cannot answer publishes the members it CAN serve and leaves the
 * rest out. `dispatchVoiceAction` then refuses the row against its declared
 * `needs`, which is the same refusal the overview already gets for a deck
 * overlay, rather than a call into a verb that is not there.
 */
export type VoicePanelChannel = { current: Partial<VoicePanelContext> | undefined };

/**
 * Everything the DECK screen is expected to serve: the context minus the voice
 * surface's own members and the overview's (PRD #1223 split the second set out;
 * see {@link VoiceOverviewContext}).
 *
 * `Omit<…, keyof VoicePanelContext | …>` rather than a second hand-written
 * list — the halves are complements by construction, so moving a member from
 * one to another is one edit and cannot leave a member served twice or not at
 * all. Before `stopVoice` existed a screen served the whole context and this
 * was `VoiceActionContext` itself.
 */
export type VoiceScreenContext = Omit<VoiceActionContext, keyof VoicePanelContext | keyof VoiceOverviewContext | keyof VoiceShellContext>;

/**
 * The members only the SHELL serves (issue #1197): closing Settings, which is
 * the shell's overlay rather than a screen's, since it opens over the overview
 * as well as the deck — published only while the sheet is open, see
 * {@link VoiceActionContext.closeSettings} — closing the topmost overlay with
 * no member of its own, read from the fence's stack, which spans both screens
 * ({@link VoiceActionContext.closeOverlay}), and switching deck (PRD #1195),
 * whose selector sits on both screens and writes the shell's settings, as
 * the Add a daemon dialog the selector opens does (PRD #2274). PRD
 * #1885's review dialog is the shell's too: it opens over the New agent dialog
 * on the overview and over the Runs screen on the deck. Its Cancel is reached
 * through `close`, so the shell publishes only its approve.
 */
type VoiceShellContext = Pick<VoiceActionContext, "closeSettings" | "closeOverlay" | "choosePaneTab" | "switchDeck" | "openAddDaemon" | "turnPage" | "approveProjectCommands" | "cancelProjectCommands" | "showApprovalRequest" | "starRepo" | "remindStarLater" | "dismissStarPrompt" | "openAntigravityIssues" | "remindAntigravityLater" | "dismissAntigravityNotice" | keyof PullRequestContext>;

/**
 * PRD #1401 — the pull request browser's members, which the SHELL serves: the
 * browser opens over the deck's pane and the overview's alike.
 */
export type PullRequestContext = Pick<VoiceActionContext, "openPullRequest" | "openPullRequestInBrowser" | "closePullRequest" | "pullRequestBack" | "openCheckedReference">;

/**
 * The members only the OVERVIEW serves (PRD #1223): opening the New agent
 * dialog, which lives on that screen because its deck step lists the fleet the
 * overview shows, and — while it is open — closing it (U5). Split out of {@link VoiceScreenContext} — which is what the
 * DECK screen publishes — so the deck is not made to serve a member it has no
 * dialog for; a dispatch of `openNewAgent` there is refused against its
 * `needs`, the way the overview refuses a deck overlay.
 */
export type VoiceOverviewContext = Pick<VoiceActionContext, "openNewAgent" | "closeNewAgent" | "openDirectory" | "goToParentDirectory" | "useThisDirectory" | "filterDirectories" | "clearDirectoryFilter" | NewAgentFormMember | "confirmStopAgent" | "confirmCloseOrchestration" | "confirmGenerateConfig" | "scrollDashboard" | "revealAgent" | "showHostMetrics" | "closeHostMetrics" | "showDashboardCounters" | "closeDashboardCounters" | "showInterruptedWork" | "hideInterruptedWork" | "interruptedDialog" | AgentCardMember>;

/** PRD #2374 — the agent cards' members, served by the overview. */
type AgentCardMember = "setOverviewMode" | "showAgentCard" | "stepAgentCard" | "setNeedsYouFilter" | "closeCardReview" | "runCardAction" | "confirmCardAction" | "toggleOverviewRowMenu" | "setCardTree" | "openCardReference" | "runCardActionBatch" | "clearCardSelection" | "toggleCardSelected" | "runSelectionBatch" | "sendCardBatch" | "closeCardBatch" | CardActionsEditorMember;

/** PRD #2374 decision 2026-11-24 — the card actions editor's members, served by the overview. */
/** PR #2491 decision 2026-11-25 — "open card actions" with several daemons and none named: the sentence, and the daemons to choose from. */
export interface CardActionsWhich {
  refused: string;
  which: { deckId: string; name: string }[];
}

export type CardActionsEditorMember = "openCardActionsEditor" | "closeCardActionsEditor" | "refreshCardActions" | "addCardAction" | "editCardAction" | "removeCardAction" | "confirmRemoveCardAction" | "hideCardAction" | "restoreCardAction" | "saveCardAction" | "cancelCardActionForm" | "chooseCardActionIcon" | "setCardActionFlag" | "setCardActionPlacement";

/** The New agent form's members, served — like the browser's — from the dialog's slot. */
type NewAgentFormMember = "chooseNewAgentDeck" | "chooseNewAgentMode" | "chooseNewAgentType" | "nameNewAgent" | "setNewAgentCommand" | "startNewAgent" | "discardNewAgent";

/**
 * What the New agent dialog publishes about its directory browser (PRD #1223),
 * and the one slot through which the INSIDE of a mounted dialog reaches voice.
 *
 * `directories` is the declaration — what the browser shows, or `undefined`
 * when it shows nothing to name — which the voice surface sends with each
 * utterance. The three functions are the browser's own moves, each refusing
 * in the dialog's words when it cannot apply.
 *
 * Written by the dialog on every render and cleared on unmount, so a reader at
 * resolve or dispatch time sees the last committed browser or nothing. The
 * overview serves the three context members by reading this slot at call time
 * (see `AgentOverview`), which is what keeps "the dialog closed during the
 * round trip" a refusal with a sentence rather than a `needs` miss.
 */
export type NewAgentVoice = Pick<VoiceActionContext, "openDirectory" | "goToParentDirectory" | "useThisDirectory" | "filterDirectories" | "clearDirectoryFilter" | NewAgentFormMember> & {
  directories: import("./bridge").VoiceDirectoriesDto | undefined;
  /**
   * The dialog's own declaration — present for as long as it is mounted, with
   * a `form` only while the form's fields are live. See `VoiceNewAgentDto`.
   */
  newAgent: import("./bridge").VoiceNewAgentDto;
  /**
   * This MOUNT of the dialog, minted once per open and never sent to Rust.
   *
   * The declaration above deliberately compares only presences, so two live
   * forms look alike however different their contents (see
   * `sameNewAgentDeclaration`), and the rows that care re-check `{deckId,
   * path}` at dispatch. `close` has nothing to re-check: it closes whatever
   * dialog is open when it lands. So a `close` resolved against one dialog
   * closed a REPLACEMENT the user had opened meanwhile, discarding its draft
   * (Qodo on PR #1235). Comparing this refuses that with `DIALOG_MOVED_ON`,
   * which is what a user who reopened the dialog should hear.
   */
  instance: string;
};
export type NewAgentVoiceChannel = { current: NewAgentVoice | undefined };

/**
 * How the overview publishes what a voice dispatch may need from it —
 * `closeNewAgent` while the dialog is open, `openNewAgent` while it is closed,
 * and the three directory members always, each refusing in words when there
 * is no browser to move — to the shell that dispatches, the way
 * {@link VoiceContextChannel} carries the deck's.
 */
export type VoiceOverviewChannel = { current: VoiceOverviewPublished | undefined };

/**
 * What the overview publishes: its members, and (PR #2491, 2026-11-25) the
 * dashboard's view and Needs you only as they stand, which the voice panel
 * declares with each utterance on the dashboard. Not a member: no action runs it.
 */
export type VoiceOverviewPublished = Partial<VoiceOverviewContext> & { dashboardState?: () => import("./bridge").VoiceDashboardDto };

/**
 * How a screen publishes its half of the context up to the host that dispatches
 * (PRD #802 M7).
 *
 * A mutable slot rather than a callback prop, and the shape is the decision. The
 * context a screen can serve is rebuilt on every render — it closes over that
 * render's setters and props — so a host holding a *copy* would dispatch through
 * stale closures. A host reading the slot at DISPATCH time always gets the last
 * committed one, and gets `undefined` the moment the screen unmounts, which is
 * exactly the question {@link dispatchVoiceAction} has to answer.
 *
 * Deliberately not a React context: the consumer is `DeckShell`, which is the
 * screen's PARENT, and a context travels the other way.
 */
export type VoiceContextChannel = { current: VoiceScreenContext | undefined };

/**
 * Dispatch the action an outcome's `invoke` names (PRD #802 M6).
 *
 * **This is voice's whole execution path, and it runs no action of its own.** It
 * looks `invoke` up in {@link VOICE_ACTIONS} and calls the same `run` the rail
 * button and the palette item call — which is what makes the pipeline's step 5,
 * *"hand the resolved action to the existing handler"*, literally true rather
 * than a description of two implementations that agree.
 *
 * **It answers `false` for two different situations, and both mean the same
 * thing to the caller: nothing ran.**
 *
 * The first is an `invoke` naming no entry. That should be unreachable —
 * `xtask/linkage-check` rule 14 fails the build on an `invoke` that resolves to
 * nothing, and the Rust pipeline refuses an action outside the table before it
 * ever gets here — so the boolean is not a second validation. It is what stops
 * the residual being *silence*: the surface can say the command did not run,
 * instead of reporting a success nothing performed.
 *
 * The second is a host that cannot serve the entry's {@link
 * VoiceActionEntry.needs}, and it is reachable by ordinary use: the overview
 * mounts no deck, so no overlay can be opened from it. **Checking before the
 * call rather than catching after it is the whole point.** M6 did neither, and
 * an unservable dispatch threw `TypeError: context.openOverlay is not a
 * function` — a string composed by the JavaScript engine, rendered by the voice
 * surface, at a user. PRD #802's central property is that the app renders every
 * sentence it shows; a caught exception message would break it just as a thrown
 * one does, which is why this is a precondition and not a `try`.
 *
 * The cast is the price of a dynamic key over entries with deliberately
 * different signatures, and it is confined to this one line rather than spread
 * across a switch with an arm per id. A switch would type-check and would also
 * be the second list of ids this design refuses to have.
 */
export function dispatchVoiceAction(invoke: string, context: VoiceDispatchContext, target: VoiceDispatchTarget): boolean {
  const entry: VoiceActionEntry | undefined = (VOICE_ACTIONS as Record<string, VoiceActionEntry>)[invoke];
  if (!entry) return false;
  /* A member is a call, or — `interruptedDialog` (PRD #1790 stage 2) — an object of calls. */
  if (entry.needs.some((member) => typeof context[member] !== "function" && (typeof context[member] !== "object" || context[member] === null))) return false;
  (entry.run as (...args: unknown[]) => void)(context, target);
  return true;
}
