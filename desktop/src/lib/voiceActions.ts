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
 * **Not "one capability, one dispatch path", which is what M2's commit body and
 * an earlier draft of this comment said and is not true.** Five `no_voice`
 * capabilities also have a second, in-panel `setState` path in `App.tsx`, and
 * naming them here is the point — a rediscovered list is a finding, a written
 * one is a known residual:
 *
 * - `focusAgent` — the agent tile's own `onSelect` calls `setSelectedAgentId`;
 * - `toggleEvidenceDrawer` — the workspace header's Evidence button, and the
 *   evidence row's select-and-open;
 * - `openWorkflowOrder` — the run-graph "Edit loop" button (twice) and
 *   `ProjectsPanel`'s `onConfigureWorkflow`;
 * - `openProjects` — `WorkflowPanel`'s `onChooseProject`;
 * - `openAgentProfiles` — `EmptyDeck`'s `onProfiles`.
 *
 * (`grep -n 'setSelectedAgentId\|setEvidenceOpen\|setWorkflowOpen\|setProjectsOpen\|setProfilesOpen' desktop/src/App.tsx`
 * finds them; line numbers are deliberately not quoted, since they rot.)
 *
 * **The load-bearing property survives the narrowing, which is why the code was
 * not re-routed to make the wider claim true.** None of those five is
 * voice-reachable — each carries a `no_voice` reason — so voice has exactly one
 * execution path, {@link dispatchVoiceAction}, and acquires no second one. What
 * is false is only the stronger claim that every capability in this file has a
 * single dispatch site. Re-routing eight call sites to recover it would be
 * regression risk for no functional gain.
 *
 * **If a later PRD gives any of those five a table row, closing its second path
 * is that PRD's work** — and it has to be, because the moment a capability is
 * voice-reachable, a second path is a behaviour voice cannot see. That is the
 * cost of leaving them, stated rather than discovered.
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
  /** Flip the evidence drawer. */
  toggleEvidence: () => void;
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
   * Aim the microphone at one agent: from here every utterance is typed into
   * that agent's prompt rather than resolved as a command (PRD #802 D6).
   *
   * Served by the voice surface because the microphone is the surface's, and
   * because the exit — a phrase, a pending send, a countdown — is state no
   * screen has anywhere to keep.
   */
  startDictation: (target: VoiceDispatchTarget) => void;
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
    label: "Aim voice at one agent and type what it hears",
    voice: true,
    needs: ["navigate", "startDictation"],
    /**
     * Opening the pane is HALF the action, not a convenience beside it.
     *
     * The requirement is that dictated words land in a **visible** input the
     * user can see and edit — never a hidden buffer — and on the overview no
     * terminal is mounted at all (PRD #745's commitment). So this opens the
     * agent it is about to type into, and the two together are what makes the
     * visibility true on every screen rather than only on the deck.
     *
     * `selectAgent` is deliberately not read even where a host offers it:
     * `navigate` to an agent view is what puts the pane on screen, and the
     * tile selection underneath it is not something dictation has an opinion
     * about.
     */
    run: (context: Pick<VoiceActionContext, "navigate" | "startDictation">, target: VoiceDispatchTarget) => {
      context.navigate({ kind: "agent", deckId: target.deckId, agentId: target.agentId, from: target.from });
      context.startDictation(target);
    },
  },

  openOverview: {
    label: "Show the agent overview",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "overview" }),
  },

  openDeck: {
    label: "Go back to the deck",
    voice: true,
    needs: ["navigate"],
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "deck" }),
  },

  closeAgentView: {
    label: "Close the open agent view",
    voice: true,
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

  // -- the rest of the rail and the palette -------------------------------

  openProjects: {
    label: "Manage projects",
    no_voice: "opens a picker over daemon-supplied project paths, and the table has no resolver kind that can turn a spoken phrase into one — a row could open the panel and then leave the user inside a list voice cannot choose from, which is a worse dead end than having no command",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("projects"),
  },

  openPromptLibrary: {
    label: "Open the prompt library",
    no_voice: "a browse-and-edit surface: choosing, adding, editing and removing a stored prompt are all beyond this PRD's navigation-only slice, so the command would open a panel and stop",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("prompts"),
  },

  openAgentProfiles: {
    label: "Open agent profiles",
    no_voice: "opens the form that sets each role's model and permissions — the configuration surface PRD #802 D5 puts behind confirmation, so exposing the door before the confirmation flow exists would invite the misfire D5 is about",
    needs: ["openOverlay"],
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("profiles"),
  },

  openWorkflowOrder: {
    label: "Edit workflow order",
    no_voice: "the editor it opens enables, skips, reorders and LAUNCHES roles; launching an orchestration starts agents, and nothing in this slice starts anything",
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
    run: (context: Pick<VoiceActionContext, "toggleEvidence">) => context.toggleEvidence(),
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
export type VoicePanelContext = Pick<VoiceActionContext, "stopVoice" | "showVoiceCommands" | "startDictation">;
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
 * Everything a SCREEN is expected to serve: the context minus the voice
 * surface's own members.
 *
 * `Omit<…, keyof VoicePanelContext>` rather than a second hand-written list —
 * the two halves are complements by construction, so moving a member from one
 * to the other is one edit and cannot leave a member served twice or not at
 * all. Before `stopVoice` existed a screen served the whole context and this
 * was `VoiceActionContext` itself.
 */
export type VoiceScreenContext = Omit<VoiceActionContext, keyof VoicePanelContext>;

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
