import { useSyncExternalStore } from "react";

import type { CardResults, PlacedCard } from "./agentCards";
import type { VoiceActionContext } from "./voiceActions";
import type { EndedRole } from "../types";

/**
 * The members the Card panel's own controls dispatch through: its question
 * answers and action icons, its reference badges, and the pane header's
 * previous, next and Next needing you.
 */
export type PaneCardControls = Pick<VoiceActionContext, "runCardAction" | "openCardReference" | "stepAgentCard"> & Partial<Pick<VoiceActionContext, "reportRefused">>;

/**
 * PR #2491 decision 2026-11-25 — what the agent pane needs from the
 * dashboard's cards to draw its Card panel. The overview holds the cards and
 * the pane is its sibling in the shell, so the overview publishes this here
 * and the pane reads it ({@link usePaneCards}): the panel shows exactly the
 * card the dashboard shows, and presses go through the same members.
 */
export interface PaneCardsView {
  /** Whether a deck's daemon offers cards: the pane has a Card button only then. */
  shownFor: (deckId: string) => boolean;
  /** One agent's card, when it has one. */
  cardFor: (deckId: string, agentId: string, incarnation?: string) => PlacedCard | undefined;
  /** The last answer to each action pressed on one card. */
  resultsFor: (card: PlacedCard) => CardResults;
  /** Whether a deck's daemon builds the card tree, so its checked references are its own. */
  treeFor: (deckId: string) => boolean;
  /** Issue #1965 — the exited orchestration role a card is for. */
  endedFor: (card: PlacedCard) => EndedRole | undefined;
  /** Why the last press did not happen, said in the panel, which covers the dashboard's banner. */
  notice: string | undefined;
  controls: PaneCardControls;
}

/** One shell's channel: the overview writes, the pane reads. */
export interface PaneCardsChannel {
  get: () => PaneCardsView | undefined;
  set: (view: PaneCardsView | undefined) => void;
  subscribe: (listener: () => void) => () => void;
}

export function createPaneCardsChannel(): PaneCardsChannel {
  let current: PaneCardsView | undefined;
  const listeners = new Set<() => void>();
  return {
    get: () => current,
    set: (view) => {
      if (view === current) return;
      current = view;
      listeners.forEach((listener) => listener());
    },
    subscribe: (listener) => {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
  };
}

const NOTHING = () => () => undefined;
const NONE = () => undefined;

/** The cards the overview published, re-read whenever it publishes; `undefined` with no channel or before the first. */
export function usePaneCards(channel: PaneCardsChannel | undefined): PaneCardsView | undefined {
  return useSyncExternalStore(channel?.subscribe ?? NOTHING, channel?.get ?? NONE, channel?.get ?? NONE);
}

/** Where the pane remembers whether the user last had the Card panel open. */
export const CARD_PANEL_STORAGE_KEY = "dot-agent-deck.agent-pane.card-panel";

/** The user's last choice, open or closed; closed when nothing was chosen or the storage refuses. */
export function readCardPanelChoice(): boolean {
  try {
    return window.localStorage.getItem(CARD_PANEL_STORAGE_KEY) === "open";
  } catch {
    return false;
  }
}

/** Remember the user's choice; a storage that refuses keeps it for this pane only. */
export function writeCardPanelChoice(open: boolean): void {
  try {
    window.localStorage.setItem(CARD_PANEL_STORAGE_KEY, open ? "open" : "closed");
  } catch {
    /* Remembered for as long as the pane is shown. */
  }
}
