import { useCallback, useMemo, useRef, useState } from "react";

import { actionBySpokenLabel, cardTarget, disabledReasonSentence, placeCards, placedCardKey, readOverviewMode, stepCard, writeOverviewMode, type CardOverlayStep, type CardResults, type CardStep, type OverviewMode, type PlacedCard } from "../lib/agentCards";
import { newCardRequestId } from "../lib/bridge";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { AgentCardRef, VoiceActionContext } from "../lib/voiceActions";
import { desktopFeaturesOf, type AgentCardAction, type DeckRuntimeState } from "../types";

/** What a card member says when the cards are not shown (the experimental flag is off). */
export const CARDS_HIDDEN = "Agent cards are an experimental view, and this app shows them only while the experimental flag is on.";
/** …when the card it names is not on the dashboard any more. */
export const CARD_GONE = "That agent has no card on the dashboard any more, so nothing was done.";
/** …when a card step or close is asked for with no card open. */
export const NO_CARD_OPEN = "No card is open.";
/** …when no card needs the user. */
export const NO_CARD_NEEDS_YOU = "No other card needs you right now.";
/** …when there are no cards to move between. */
export const NO_CARDS = "There are no cards to show.";

/** The card members the overview serves, every one of them. */
export type AgentCardControls = Pick<VoiceActionContext, "setOverviewMode" | "showAgentCard" | "stepAgentCard" | "setNeedsYouFilter" | "closeAgentCard" | "runCardAction" | "replyToAgentCard" | "confirmCardAction" | "toggleOverviewRowMenu">;

/** The overlay as the overview renders it. */
export interface CardOverlayView {
  card: PlacedCard | undefined;
  lastName: string;
  position: number;
  count: number;
  step: CardOverlayStep | undefined;
  results: CardResults;
}

/** What {@link useAgentCardsView} hands the overview. */
export interface AgentCardsView {
  /** Whether the experimental flag shows the cards at all. */
  shown: boolean;
  /** The view on screen: always `table` while the cards are hidden. */
  mode: OverviewMode;
  needsYouOnly: boolean;
  /** Every card of every answering deck, in display order. */
  placed: PlacedCard[];
  /** The deck ids that have cards (their daemon supports them). */
  decksWithCards: ReadonlySet<string>;
  overlay: CardOverlayView | undefined;
  /** The table row whose ⋯ menu is open. */
  rowMenu: string | undefined;
  controls: AgentCardControls;
  /** The members published to voice: `closeAgentCard` only while a card is open. */
  voice: Partial<AgentCardControls>;
  /** The card of one dashboard agent, when it has one. */
  cardFor: (deckId: string, agentId: string, incarnation?: string) => PlacedCard | undefined;
  /** The last answer to each action pressed on one card. */
  resultsFor: (card: PlacedCard) => CardResults;
}

function sameIncarnation(card: PlacedCard, incarnation: string | undefined): boolean {
  return incarnation === undefined || card.entry.instance_id === "" || card.entry.instance_id === incarnation;
}

/**
 * PRD #2374 — the dashboard's cards view: which view is on screen, the Needs
 * you filter, the open card overlay and its step, each table row's menu, and
 * the card members every control and voice dispatch through. It renders and
 * sends what the daemon decided; the only things decided here are the user's
 * own choices (view, filter, which card is open).
 */
export function useAgentCardsView({ runtime, decks, openAgent }: {
  runtime: Pick<DeckRuntimeState, "desktopFeatures" | "agentCards" | "invokeCardAction">;
  decks: readonly { deckId: string | undefined; connected: boolean }[];
  /** Open an agent's pane — a card's `open` action. */
  openAgent: (target: { deckId: string; agentId: string; incarnation?: string }) => void;
}): AgentCardsView {
  const shown = desktopFeaturesOf(runtime).showAgentCards && runtime.agentCards !== undefined;
  const [storedMode, setStoredMode] = useState<OverviewMode>(readOverviewMode);
  const mode: OverviewMode = shown ? storedMode : "table";
  const [needsYouOnly, setNeedsYouOnly] = useState(false);
  const [open, setOpen] = useState<{ key: string; name: string; step?: CardOverlayStep }>();
  const [results, setResults] = useState<Readonly<Record<string, CardResults>>>({});
  const [rowMenu, setRowMenu] = useState<string>();
  const placed = useMemo(() => (shown ? placeCards(decks, runtime.agentCards) : []), [decks, runtime.agentCards, shown]);
  const decksWithCards = useMemo(() => new Set(shown ? Object.keys(runtime.agentCards ?? {}) : []), [runtime.agentCards, shown]);
  /* The latest committed state, for members a dispatch calls after a render. */
  const latest = useRef({ placed, open, shown });
  latest.current = { placed, open, shown };

  const cardFor = useCallback((deckId: string, agentId: string, incarnation?: string) => (
    placed.find((card) => card.deckId === deckId && card.entry.agent_id === agentId && sameIncarnation(card, incarnation))
  ), [placed]);
  const find = useCallback((ref: AgentCardRef) => (
    latest.current.placed.find((card) => card.deckId === ref.deckId && card.entry.agent_id === ref.agentId && sameIncarnation(card, ref.incarnation))
  ), []);
  const remember = useCallback((next: OverviewMode) => {
    setStoredMode(next);
    writeOverviewMode(next);
  }, []);
  const invoke = runtime.invokeCardAction;
  const send = useCallback((card: PlacedCard, action: AgentCardAction, options: { confirm?: boolean; text?: string }) => {
    const cardKey = placedCardKey(card);
    const record = (answer: CardResults[string]) => setResults((current) => ({ ...current, [cardKey]: { ...current[cardKey], [action.id]: answer } }));
    if (!invoke) {
      record({ outcome: "failed", message: "this app cannot reach the daemon for card actions" });
      return;
    }
    record({ outcome: "pending" });
    void invoke(cardTarget(card), action.id, { ...options, requestId: newCardRequestId() }).then(
      (answer) => record(answer),
      (cause: unknown) => record({ outcome: "failed", message: cause instanceof Error ? cause.message : String(cause) }),
    );
  }, [invoke]);

  const controls = useMemo<AgentCardControls>(() => ({
    setOverviewMode: (next) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      remember(next);
      return undefined;
    },
    showAgentCard: (ref) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      setRowMenu(undefined);
      setOpen({ key: placedCardKey(card), name: card.entry.name });
      return undefined;
    },
    stepAgentCard: (step: CardStep) => {
      const { open: current, placed: order } = latest.current;
      if (!current) return NO_CARD_OPEN;
      const at = stepCard(order, order.findIndex((card) => placedCardKey(card) === current.key), step);
      if (at === undefined) return step === "needs-you" ? NO_CARD_NEEDS_YOU : NO_CARDS;
      setOpen({ key: placedCardKey(order[at]), name: order[at].entry.name });
      return undefined;
    },
    setNeedsYouFilter: (on) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      setNeedsYouOnly(on);
      // The filter is the Cards view's: asking for it shows that view.
      if (on) remember("cards");
      return undefined;
    },
    closeAgentCard: () => {
      if (!latest.current.open) return NO_CARD_OPEN;
      setOpen(undefined);
      return undefined;
    },
    runCardAction: (ref, wanted) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const name = displayText(card.entry.name, DISPLAY_LIMITS.name);
      const action = wanted.actionId !== undefined
        ? card.entry.actions.find((candidate) => candidate.id === wanted.actionId)
        : actionBySpokenLabel(card.entry, wanted.label ?? "");
      if (!action) return wanted.label !== undefined ? `${name}'s card has no single action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.` : CARD_GONE;
      setRowMenu(undefined);
      if (!action.enabled) return `${displayText(action.label, DISPLAY_LIMITS.name)} is disabled: ${disabledReasonSentence(action.disabled_reason)}.`;
      if (action.kind === "open") {
        openAgent({ deckId: card.deckId, agentId: card.entry.agent_id, ...(card.entry.instance_id ? { incarnation: card.entry.instance_id } : {}) });
        return undefined;
      }
      if (action.text_input) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "reply" } });
        return undefined;
      }
      if (action.confirm) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "confirm", actionId: action.id } });
        return undefined;
      }
      send(card, action, {});
      return undefined;
    },
    replyToAgentCard: (ref, text) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const reply = card.entry.actions.find((candidate) => candidate.id === "reply");
      if (!reply) return `${displayText(card.entry.name, DISPLAY_LIMITS.name)}'s card takes no reply.`;
      if (!reply.enabled) return `Reply is disabled: ${disabledReasonSentence(reply.disabled_reason)}.`;
      send(card, reply, { text });
      setOpen((current) => (current && current.key === placedCardKey(card) ? { key: current.key, name: current.name } : current));
      return undefined;
    },
    confirmCardAction: () => {
      const { open: current, placed: order } = latest.current;
      if (!current || current.step?.kind !== "confirm") return;
      const card = order.find((candidate) => placedCardKey(candidate) === current.key);
      const actionId = current.step.actionId;
      const action = card?.entry.actions.find((candidate) => candidate.id === actionId);
      if (!card || !action || !action.enabled) return;
      send(card, action, { confirm: true });
      setOpen({ key: current.key, name: current.name });
    },
    toggleOverviewRowMenu: (rowKey) => setRowMenu((current) => (current === rowKey ? undefined : rowKey)),
  }), [find, openAgent, remember, send]);

  const overlay = useMemo<CardOverlayView | undefined>(() => {
    if (!open || !shown) return undefined;
    const at = placed.findIndex((card) => placedCardKey(card) === open.key);
    return {
      card: at < 0 ? undefined : placed[at],
      lastName: open.name,
      position: at + 1,
      count: placed.length,
      step: open.step,
      results: results[open.key] ?? {},
    };
  }, [open, placed, results, shown]);

  const overlayOpen = overlay !== undefined;
  const voice = useMemo<Partial<AgentCardControls>>(() => (
    overlayOpen ? controls : { ...controls, closeAgentCard: undefined }
  ), [controls, overlayOpen]);

  const resultsFor = useCallback((card: PlacedCard) => results[placedCardKey(card)] ?? {}, [results]);
  return { shown, mode, needsYouOnly, placed, decksWithCards, overlay, rowMenu, controls, voice, cardFor, resultsFor };
}
