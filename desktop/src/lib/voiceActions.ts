import type { DeckView } from "../types";

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
 * tile's own selection (`focusAgent`), the workspace header's Evidence button
 * and an evidence row's select-and-open (`toggleEvidenceDrawer`), the run
 * graph's Edit loop controls and Projects' Configure workflow
 * (`openWorkflowOrder`), the workflow editor's Choose one (`openProjects`), and
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
 * `xtask/linkage-check` rule 13 reads this file as text, the way
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
export type DeckOverlay = "projects" | "prompts" | "profiles" | "workflow" | "settings";

/** Which agent's pane to open, and which screen it is opened over. */
export type AgentViewTarget = {
  deckId: string;
  agentId: string;
  from: "deck" | "overview";
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
   */
  switchDeck: (selection: string) => string | undefined;
  /**
   * Close the Settings sheet (issue #1197).
   *
   * **Served by the SHELL, and only while Settings is OPEN**, for
   * {@link dismissVoiceOverlay}'s reason: Settings is a shell-level overlay
   * boolean (`useShellOverlays`), not a `DeckView`, so {@link closeTopmost}
   * reads *"Settings is open"* as this member's presence. The shell and not a
   * screen, because the sheet opens over the overview as well as the deck.
   */
  closeSettings: () => void;
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
   * Command is never DICTATED: it is the field that executes, so the only
   * thing voice puts there is a registry default (`commands.toml` has the
   * argument).
   */
  chooseNewAgentMode: (target: VoiceDispatchTarget) => string | undefined;
  chooseNewAgentType: (target: VoiceDispatchTarget) => string | undefined;
  nameNewAgent: (target: VoiceDispatchTarget) => string | undefined;
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
   * PRD #802 D5 — the two voice members that lead to a STOP, and the one rule
   * they share: **each can only OPEN a confirmation.** Neither runs a deck
   * action; the confirmation's own button does, pressed by the user. So a
   * misheard or over-confident answer costs a dialog the user dismisses, never
   * an agent stopped that nobody meant to stop. They keep it where the start
   * did not because they are destructive, their target may be off-screen, and
   * an orchestration close takes several roles at once.
   *
   * - `confirmStopAgent` — the overview row's Stop, for the agent an
   *   `agent_ref` resolved to: the same confirmation that button opens.
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
};

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
    run: (context: Pick<VoiceActionContext, "navigate"> & Partial<Pick<VoiceActionContext, "selectAgent">>, target: AgentViewTarget) => {
      context.selectAgent?.(target.agentId);
      /* The three view members named rather than spread. `VoiceDispatchTarget`
         carries `agentLabel` as well, which belongs to the dictation row and
         not in a `DeckView` — a spread would put it in the app's view state,
         where nothing reads it and everything compares it. */
      context.navigate({ kind: "agent", deckId: target.deckId, agentId: target.agentId, from: target.from });
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
     * dialog is open, and for `closeSettings`, which the shell publishes only
     * while Settings is open.
     *
     * **Settings is checked LAST, below the agent's pane** (issue #1197). The
     * sheet is a shell overlay that stays open across a pane opened over it —
     * by voice, or from the rail — and that pane is then what the user is
     * looking at, so `close` takes the pane first and Settings on the next
     * utterance. Before Settings moved to the shell it was not in this order
     * at all, and `close` with it open answered "nothing to close".
     */
    run: (
      context: Pick<VoiceActionContext, "closeAgentView" | "reportNothingToClose" | "reportRefused"> & Partial<Pick<VoiceActionContext, "dismissVoiceOverlay" | "closeNewAgent" | "closeSettings">>,
      target: VoiceDispatchTarget,
    ) => {
      if (context.dismissVoiceOverlay) return context.dismissVoiceOverlay();
      if (context.closeNewAgent) {
        const refused = context.closeNewAgent();
        if (refused !== undefined) context.reportRefused(refused);
        return;
      }
      if (target.agentViewOpen) return context.closeAgentView();
      if (context.closeSettings) return context.closeSettings();
      context.reportNothingToClose();
    },
  },

  openOverview: {
    label: "Show the agent overview",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "overview" }),
  },

  /**
   * Voice-reachable, and the one row whose screen issue #1198 hides by
   * default: while the flag is off the crate offers it `callable: false`
   * (`voice::schema::hidden_by_flag`) and `DeckShell` refuses it at dispatch,
   * so the row stays in the table and the door stays shut.
   */
  openDeck: {
    label: "Go back to the deck",
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
    label: "Switch which deck the app is showing",
    voice: true,
    needs: ["switchDeck"],
    run: (context: Pick<VoiceActionContext, "switchDeck"> & Partial<Pick<VoiceActionContext, "reportRefused">>, target: { deckSelection?: string }) => {
      const refused = context.switchDeck(target.deckSelection ?? "");
      if (refused !== undefined) context.reportRefused?.(refused);
    },
  },

  // -- the rest of the rail and the palette -------------------------------

  openProjects: {
    label: "Manage projects",
    no_voice: "hidden unless the experimental flag is on (issue #1198), so by default a row would be a spoken door to a panel the app does not show; and with the flag on, it opens a picker over daemon-supplied project paths, and the table has no resolver kind that can turn a spoken phrase into one — a row could open the panel and then leave the user inside a list voice cannot choose from, which is a worse dead end than having no command",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("projects"),
  },

  openPromptLibrary: {
    label: "Open the prompt library",
    no_voice: "hidden unless the experimental flag is on (issue #1198), so by default a row would be a spoken door to a panel the app does not show; and with the flag on, it is a browse-and-edit surface: choosing, adding, editing and removing a stored prompt are all beyond this PRD's navigation-only slice, so the command would open a panel and stop",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("prompts"),
  },

  openAgentProfiles: {
    label: "Open agent profiles",
    no_voice: "hidden unless the experimental flag is on (issue #1198), so by default a row would be a spoken door to a panel the app does not show; and with the flag on, it opens the form that sets each role's model and permissions — the configuration surface PRD #802 D5 puts behind confirmation, so exposing the door before the confirmation flow exists would invite the misfire D5 is about",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("profiles"),
  },

  openWorkflowOrder: {
    label: "Edit workflow order",
    no_voice: "hidden unless the experimental flag is on (issue #1198), so by default a row would be a spoken door to a panel the app does not show; and with the flag on, the editor it opens enables, skips, reorders and LAUNCHES roles; launching an orchestration starts agents, and nothing in this slice starts anything",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("workflow"),
  },

  openSettings: {
    label: "Open settings",
    voice: true,
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("settings"),
  },

  showRuns: {
    label: "Show the running agents",
    no_voice: "clears whichever overlays happen to be open and reveals the deck underneath, so with nothing open it does nothing at all and no honest report sentence can be written for it; `open_deck` is the row that means \"show me the terminals\"",
    needs: ["closeOverlays"],
    run: (context: Pick<VoiceActionContext, "closeOverlays">) => context.closeOverlays(),
  },

  toggleEvidenceDrawer: {
    label: "Show or hide the evidence drawer",
    no_voice: "a toggle, and the table cannot see which way it is pointing — the drawer is a `ControlDeck` boolean rather than a screen — so \"show the evidence\" and \"hide the evidence\" would both flip it and one of the two would be wrong every time",
    needs: ["toggleEvidence"],
    /** No target flips it: the header's Evidence button and the palette entry.
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
    label: "Put the caret in the coordinator's terminal",
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
    label: "Start a new agent on a chosen deck",
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

  /* PRD #1223 — the three `new_agent_form` rows: Mode, Agent and Name. None
     starts anything, so none is in PRD #802 D5's confirmation set; the manual
     chips, picker and Name input are unchanged and call the same functions.
     Command has no entry here on purpose. */
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

  /* Issue #1263 — the deck field. It starts nothing, so it is outside PRD
     #802 D5's set; the manual path (a click, or Enter on a highlighted row)
     calls the same `chooseDeck`. */
  chooseNewAgentDeck: {
    label: "Choose the deck in the New agent dialog",
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

  /* PRD #802 D5 — the two rows that STOP something. Each entry calls a member
     that can only OPEN a confirmation (see the members' own comment on
     `VoiceActionContext`), so voice never reaches `runAction` for a stop: the
     confirmation's button does, pressed by hand. The manual controls — a
     row's Stop, a card's Close — keep exactly the behaviour they had and do
     not route through these. */

  confirmStopAgent: {
    label: "Ask to stop one agent on the overview",
    voice: true,
    needs: ["confirmStopAgent", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmStopAgent" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmStopAgent(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },

  confirmCloseOrchestration: {
    label: "Ask to close an orchestration on the overview",
    voice: true,
    needs: ["confirmCloseOrchestration", "reportRefused"],
    run: (context: Pick<VoiceActionContext, "confirmCloseOrchestration" | "reportRefused">, target: VoiceDispatchTarget) => {
      const refused = context.confirmCloseOrchestration(target);
      if (refused !== undefined) context.reportRefused(refused);
    },
  },
} satisfies Record<string, VoiceActionEntry>;

/** Every action id, as the guard and the command table spell them. */
export type VoiceActionId = keyof typeof VOICE_ACTIONS;

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
   * The child directory to open — the deck's own path a row's `dir_ref` param
   * resolved to, against the browser's children on screen (PRD #1223).
   */
  directoryPath?: string;
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
 * # The absent members are checked now, and rule 13 still does not check them
 *
 * The guard proves an `invoke` names an entry and that the entry is classified.
 * It says nothing about whether the HOST can serve the context that entry's `run`
 * reads — those are different questions, and rule 13 answers only the first. What
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
export type VoicePanelContext = Pick<VoiceActionContext, "stopVoice" | "showVoiceCommands" | "typeIntoAgent" | "submitAgentPrompt" | "dismissVoiceOverlay" | "reportNothingToClose" | "reportRefused">;
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
 * {@link VoiceActionContext.closeSettings} — and switching deck (PRD #1195),
 * whose selector sits on both screens and writes the shell's settings.
 */
export type VoiceShellContext = Pick<VoiceActionContext, "closeSettings" | "switchDeck">;

/**
 * The members only the OVERVIEW serves (PRD #1223): opening the New agent
 * dialog, which lives on that screen because its deck step lists the fleet the
 * overview shows, and — while it is open — closing it (U5). Split out of {@link VoiceScreenContext} — which is what the
 * DECK screen publishes — so the deck is not made to serve a member it has no
 * dialog for; a dispatch of `openNewAgent` there is refused against its
 * `needs`, the way the overview refuses a deck overlay.
 */
export type VoiceOverviewContext = Pick<VoiceActionContext, "openNewAgent" | "closeNewAgent" | "openDirectory" | "goToParentDirectory" | "useThisDirectory" | NewAgentFormMember | "confirmStopAgent" | "confirmCloseOrchestration">;

/** The New agent form's members, served — like the browser's — from the dialog's slot. */
export type NewAgentFormMember = "chooseNewAgentDeck" | "chooseNewAgentMode" | "chooseNewAgentType" | "nameNewAgent" | "startNewAgent" | "discardNewAgent";

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
export type NewAgentVoice = Pick<VoiceActionContext, "openDirectory" | "goToParentDirectory" | "useThisDirectory" | NewAgentFormMember> & {
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
export type VoiceOverviewChannel = { current: Partial<VoiceOverviewContext> | undefined };

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
 * `xtask/linkage-check` rule 13 fails the build on an `invoke` that resolves to
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
  if (entry.needs.some((member) => typeof context[member] !== "function")) return false;
  (entry.run as (...args: unknown[]) => void)(context, target);
  return true;
}
