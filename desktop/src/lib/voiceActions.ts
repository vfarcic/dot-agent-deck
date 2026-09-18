import type { DeckView } from "../types";

/**
 * PRD #802 M2 — the frontend action registry, and the app's one dispatch seam
 * for the rail and the command palette.
 *
 * # Why this exists at all
 *
 * The voice command table's `invoke` column names an entry **here**, not a
 * `#[tauri::command]`. That correction is PRD #802's most consequential one and
 * it was measured rather than assumed: none of the thirteen registered Tauri
 * commands performs a navigation, because navigation in this app is React state
 * — `setView`, a `useState` boolean, a selected agent id. Voice "produces an
 * action and dispatches it where a click dispatches one", and a click is
 * dispatched in the frontend.
 *
 * So an entry is **not** a description of a control. It IS the control: the rail
 * buttons, the palette items, the agent tile's open/close pair and the
 * overview's row all run through `VOICE_ACTIONS[id].run(...)`. That is the
 * property that earns the capability definition M3's guard checks — an entry
 * cannot be deleted without breaking a control, and a control reachable from the
 * rail or the palette cannot exist without an entry. A registry beside the app
 * rather than inside it would be a checked-in list under a better name.
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
  run: (...args: never[]) => void;
  /** Set when `commands.toml` carries a row whose `invoke` names this entry. */
  voice?: true;
  /** Why this capability is not a spoken command. Mutually exclusive with `voice`. */
  no_voice?: string;
};

export const VOICE_ACTIONS = {
  // -- the four the command table names today -----------------------------

  openAgent: {
    label: "Open one agent's pane over the current screen",
    voice: true,
    /**
     * Opening SELECTS as well, on the deck. That is not decoration: one pane is
     * on screen, so it is the selected one, and it settles the narrow-viewport
     * rule that hides every unselected tile. The overview has no tile to select
     * and passes no `selectAgent`, which is why the member is optional here and
     * required nowhere else.
     */
    run: (context: Pick<VoiceActionContext, "navigate"> & Partial<Pick<VoiceActionContext, "selectAgent">>, target: AgentViewTarget) => {
      context.selectAgent?.(target.agentId);
      context.navigate({ kind: "agent", ...target });
    },
  },

  openOverview: {
    label: "Show the agent overview",
    voice: true,
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "overview" }),
  },

  openDeck: {
    label: "Go back to the deck",
    voice: true,
    run: (context: Pick<VoiceActionContext, "navigate">) => context.navigate({ kind: "deck" }),
  },

  closeAgentView: {
    label: "Close the open agent view",
    voice: true,
    /** The VIEW, never the pane. The agent keeps running and keeps its terminal. */
    run: (context: Pick<VoiceActionContext, "closeAgentView">) => context.closeAgentView(),
  },

  // -- the rest of the rail and the palette -------------------------------

  openProjects: {
    label: "Manage projects",
    no_voice: "opens a picker over daemon-supplied project paths, and the table has no resolver kind that can turn a spoken phrase into one — a row could open the panel and then leave the user inside a list voice cannot choose from, which is a worse dead end than having no command",
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("projects"),
  },

  openPromptLibrary: {
    label: "Open the prompt library",
    no_voice: "a browse-and-edit surface: choosing, adding, editing and removing a stored prompt are all beyond this PRD's navigation-only slice, so the command would open a panel and stop",
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("prompts"),
  },

  openAgentProfiles: {
    label: "Open agent profiles",
    no_voice: "opens the form that sets each role's model and permissions — the configuration surface PRD #802 D5 puts behind confirmation, so exposing the door before the confirmation flow exists would invite the misfire D5 is about",
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("profiles"),
  },

  openWorkflowOrder: {
    label: "Edit workflow order",
    no_voice: "the editor it opens enables, skips, reorders and LAUNCHES roles; launching an orchestration starts agents, and nothing in this slice starts anything",
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("workflow"),
  },

  openSettings: {
    label: "Open settings",
    no_voice: "reserved for PRD #802 M8, which adds the first new command by editing `commands.toml` and its fixtures and nothing else — claiming the row here would spend the one-file proof before it has been made",
    run: (context: Pick<VoiceActionContext, "openOverlay">) => context.openOverlay("settings"),
  },

  showRuns: {
    label: "Show the running agents",
    no_voice: "clears whichever overlays happen to be open and reveals the deck underneath, so with nothing open it does nothing at all and no honest report sentence can be written for it; `open_deck` is the row that means \"show me the terminals\"",
    run: (context: Pick<VoiceActionContext, "closeOverlays">) => context.closeOverlays(),
  },

  toggleEvidenceDrawer: {
    label: "Show or hide the evidence drawer",
    no_voice: "a toggle, and the table cannot see which way it is pointing — the drawer is a `ControlDeck` boolean rather than a screen — so \"show the evidence\" and \"hide the evidence\" would both flip it and one of the two would be wrong every time",
    run: (context: Pick<VoiceActionContext, "toggleEvidence">) => context.toggleEvidence(),
  },

  focusAgent: {
    label: "Select one agent's tile",
    /**
     * ONE entry taking the agent as a parameter, exactly as `openAgent` does,
     * rather than one entry per live agent. The palette renders N items and all
     * of them dispatch through here — which is what leaves the guard a fixed
     * key to check, since per-agent entries do not exist at compile time.
     */
    no_voice: "selects a tile without enlarging it, which is a pointer and keyboard affordance rather than something a supervisor says out loud; `open_agent` is the spoken form of \"show me that one\", and offering both would make every such utterance ambiguous for the model",
    run: (context: Pick<VoiceActionContext, "selectAgent">, target: AgentTarget) => context.selectAgent(target.agentId),
  },

  messageCoordinator: {
    label: "Put the caret in the coordinator's terminal",
    no_voice: "moves the caret so the operator can type to the orchestration's start role; dictating INTO an agent is PRD #802 D6, and a command that only moved the caret would promise an input path voice cannot finish",
    run: (context: Pick<VoiceActionContext, "focusTerminal">, target: AgentTarget) => context.focusTerminal(target.agentId),
  },

  advanceFixture: {
    label: "Advance the fixture loop one node",
    no_voice: "a fixture-mode debug affordance: it exists only while the app is driven by `FixtureDeckBridge` and moves a deterministic demo loop, so it is not a capability of a shipped build and has no user to speak to",
    run: (context: Pick<VoiceActionContext, "advanceFixture">) => context.advanceFixture(),
  },
} satisfies Record<string, VoiceActionEntry>;

/** Every action id, as the guard and the command table spell them. */
export type VoiceActionId = keyof typeof VOICE_ACTIONS;
