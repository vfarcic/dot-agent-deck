import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { actionBySpokenLabel, cardTarget, disabledReasonSentence, placeCards, placedCardKey, pruneCardResults, readOverviewMode, sendableUnread, stepCard, writeOverviewMode, type CardOverlayStep, type CardResults, type CardStep, type OverviewMode, type PlacedCard } from "../lib/agentCards";
import { boundOpenCards, cardTree, openableReference, pathTo, pruneOpenCards, readOpenCards, writeOpenCards } from "../lib/agentCardsTree";
import { newCardRequestId } from "../lib/bridge";
import type { CardReferenceTarget } from "../lib/prBrowser";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { AgentCardRef, VoiceActionContext } from "../lib/voiceActions";
import type { AgentCardAction, DeckRuntimeState } from "../types";

/** What a card member says when the cards are not shown (this connection offers no cards). */
const CARDS_HIDDEN = "Agent cards are not available here.";
/** …when the card it names is not on the dashboard any more. */
const CARD_GONE = "That agent has no card on the dashboard any more, so nothing was done.";
/** …when a card step or close is asked for with no card open. */
const NO_CARD_OPEN = "No card is open.";
/** …when no card needs the user. */
const NO_CARD_NEEDS_YOU = "No other card needs you right now.";
/** …when there are no cards to move between. */
const NO_CARDS = "There are no cards to show.";
/** …when voice named an action whose text the agent wrote: its card opens at the review step instead of sending. */
const agentWrittenReview = (label: string, name: string) => `${name} wrote the text of ${label}, so its card is open for you to read it first: press Send to send it.`;

/** …when voice asks for a card's tree on a card with nothing under it. */
const nothingUnder = (name: string) => `${name} has no cards under it.`;
/** …when voice opens a tree whose card the dashboard filter hides. */
const treeHiddenByFilter = (name: string) => `${name}'s tree is open, but the dashboard filter hides ${name}: clear the filter to see it.`;
/** …when voice opens a tree all of whose cards the dashboard filter hides. */
const treeEmptiedByFilter = (name: string) => `${name}'s tree is open, but the dashboard filter hides every card under it: clear the filter to see them.`;
/** …when a reference is asked for and this window cannot open GitHub pages. */
const NO_REFERENCE_OPENER = "This window cannot open GitHub pages.";

/** The card members the overview serves, every one of them. */
type AgentCardControls = Pick<VoiceActionContext, "setOverviewMode" | "showAgentCard" | "stepAgentCard" | "setNeedsYouFilter" | "closeAgentCard" | "runCardAction" | "replyToAgentCard" | "confirmCardAction" | "toggleOverviewRowMenu" | "setCardTree" | "openCardReference">;

/** The overlay as the overview renders it. */
interface CardOverlayView {
  card: PlacedCard | undefined;
  lastName: string;
  position: number;
  count: number;
  step: CardOverlayStep | undefined;
  results: CardResults;
  /** Whether the open card's daemon builds the tree, so its checked references are its own (issue #2418). */
  tree: boolean;
}

/** What {@link useAgentCardsView} hands the overview. */
export interface AgentCardsView {
  /** Whether the runtime offers cards at all. */
  shown: boolean;
  /** The view on screen: always `table` while the cards are hidden. */
  mode: OverviewMode;
  needsYouOnly: boolean;
  /** Every card of every answering deck, in display order. */
  placed: PlacedCard[];
  /** The deck ids that have cards (their daemon supports them). */
  decksWithCards: ReadonlySet<string>;
  /** Issue #2418 — the cards whose tree is open, by placed key (remembered per client). */
  openCards: ReadonlySet<string>;
  /** Issue #2418 — whether a deck's daemon builds the card tree; an older one's cards are all roots. */
  treeFor: (deckId: string) => boolean;
  overlay: CardOverlayView | undefined;
  /**
   * Issue #2418 review (F2) — why the last thing pressed on a card or in its
   * overlay did not happen, until it is dismissed, the overlay closes, or a
   * reference opens: a press that did nothing says why.
   */
  notice: string | undefined;
  dismissNotice: () => void;
  /** The table row whose ⋯ menu is open. */
  rowMenu: string | undefined;
  controls: AgentCardControls;
  /**
   * {@link controls} for the cards' and the overlay's own controls, which
   * report a refusal in {@link notice}. Kept apart from `controls`, which is
   * also published to voice, where the voice panel reports refusals itself.
   */
  clickControls: AgentCardControls & Pick<VoiceActionContext, "reportRefused">;
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
export function useAgentCardsView({ runtime, decks, openAgent, openReference, hiddenByFilter }: {
  runtime: Pick<DeckRuntimeState, "agentCards" | "invokeCardAction">;
  decks: readonly { deckId: string | undefined; connected: boolean }[];
  /** Open an agent's pane — a card's `open` action. */
  openAgent: (target: { deckId: string; agentId: string; incarnation?: string }) => void;
  /** Issue #2418 — open a checked pull request or issue in the app's GitHub browser; absent where there is none. */
  openReference?: (target: CardReferenceTarget) => string | undefined;
  /** Issue #2418 — whether the dashboard filter keeps a card off screen; absent when nothing filters. */
  hiddenByFilter?: (card: PlacedCard) => boolean;
}): AgentCardsView {
  const shown = runtime.agentCards !== undefined;
  const [storedMode, setStoredMode] = useState<OverviewMode>(readOverviewMode);
  const mode: OverviewMode = shown ? storedMode : "table";
  const [needsYouOnly, setNeedsYouOnly] = useState(false);
  const [open, setOpen] = useState<{ key: string; name: string; step?: CardOverlayStep }>();
  const [results, setResults] = useState<Readonly<Record<string, CardResults>>>({});
  const [rowMenu, setRowMenu] = useState<string>();
  // voice-registry-exempt: why a press on a card did nothing, written by the controls that refused it; it opens nothing
  const [notice, setNotice] = useState<string>();
  const placed = useMemo(() => (shown ? placeCards(decks, runtime.agentCards) : []), [decks, runtime.agentCards, shown]);
  const decksWithCards = useMemo(() => new Set(shown ? Object.keys(runtime.agentCards ?? {}) : []), [runtime.agentCards, shown]);
  const treeFor = useCallback((deckId: string) => runtime.agentCards?.[deckId]?.tree === true, [runtime.agentCards]);
  const [openCards, setOpenCards] = useState<ReadonlySet<string>>(readOpenCards);
  /* The latest committed state, for members a dispatch calls after a render. */
  const latest = useRef({ placed, open, shown, treeFor, openReference, hiddenByFilter });
  latest.current = { placed, open, shown, treeFor, openReference, hiddenByFilter };
  /* Every change to the open cards is remembered; one this window cannot
     store still applies until it closes. */
  const changeOpenCards = useCallback((change: (current: ReadonlySet<string>) => ReadonlySet<string>) => {
    setOpenCards((current) => {
      const next = boundOpenCards(change(current));
      if (next !== current) writeOpenCards(next);
      return next;
    });
  }, []);
  /* The decks whose daemon answered with its cards, an answer with none
     included: the open cards they no longer list are forgotten. */
  const answering = useMemo(() => new Set(shown ? decks.flatMap((deck) => (deck.connected && deck.deckId !== undefined && runtime.agentCards?.[deck.deckId] ? [deck.deckId] : [])) : []), [decks, runtime.agentCards, shown]);
  /* A remembered open card its daemon no longer lists goes (a deck that is
     not answering keeps what it had). */
  useEffect(() => {
    if (shown) changeOpenCards((current) => pruneOpenCards(current, placed, answering));
  }, [answering, changeOpenCards, placed, shown]);
  /* Needs you only keeps every card that needs the user on screen: on, it
     opens the way to each one, and while it stays on, to each card that comes
     to need the user after — a new card, or one whose status changed. The way
     to a card it already revealed is not opened again, so a tree the user
     closed stays closed until a card under it newly needs them. */
  const revealed = useRef<ReadonlySet<string>>(new Set());
  useEffect(() => {
    if (!needsYouOnly || !shown) {
      revealed.current = new Set();
      return;
    }
    const needy = new Set(placed.filter((card) => card.entry.needs_you).map(placedCardKey));
    const fresh = [...needy].filter((cardKey) => !revealed.current.has(cardKey));
    revealed.current = needy;
    if (fresh.length === 0) return;
    const freshKeys = new Set(fresh);
    const deckIds = [...new Set(placed.map((card) => card.deckId))];
    const wanted = deckIds.flatMap((deckId) => [...pathTo(cardTree(placed.filter((card) => card.deckId === deckId), treeFor(deckId)), (node) => freshKeys.has(placedCardKey(node.card))).open]);
    if (wanted.length) changeOpenCards((current) => (wanted.every((cardKey) => current.has(cardKey)) ? current : new Set([...current, ...wanted])));
  }, [changeOpenCards, needsYouOnly, placed, shown, treeFor]);

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
  /* The answers of actions no card offers any more go (audit F5); what the
     daemon delivered stays on the card itself. */
  useEffect(() => {
    setResults((current) => pruneCardResults(current, placed));
  }, [placed]);
  const invoke = runtime.invokeCardAction;
  /* `token` is the version of the action the user was shown — the pressed
     button's, or the reviewed step's — and the daemon sends nothing for any
     other (review B2). */
  const send = useCallback((card: PlacedCard, action: AgentCardAction, options: { confirm?: boolean; text?: string; token?: string }) => {
    const cardKey = placedCardKey(card);
    const actionToken = options.token ?? action.token;
    const record = (answer: CardResults[string]) => setResults((current) => ({ ...current, [cardKey]: { ...current[cardKey], [actionToken]: answer } }));
    if (!invoke) {
      record({ outcome: "failed", message: "this app cannot reach the daemon for card actions" });
      return;
    }
    record({ outcome: "pending" });
    void invoke(cardTarget(card), action.id, {
      actionToken,
      ...(options.confirm ? { confirm: true } : {}),
      ...(options.text !== undefined ? { text: options.text } : {}),
      requestId: newCardRequestId(),
    }).then(
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
    showAgentCard: (ref, section) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      setRowMenu(undefined);
      setOpen({ key: placedCardKey(card), name: card.entry.name, ...(section === "references" ? { step: { kind: "references" } } : {}) });
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
      /* Needs you only opens the path to every card that needs the user
         (the effect above, which also follows the cards while it is on). */
      setNeedsYouOnly(on);
      // The filter is the Cards view's: asking for it shows that view.
      if (on) remember("cards");
      return undefined;
    },
    closeAgentCard: () => {
      if (!latest.current.open) return NO_CARD_OPEN;
      setOpen(undefined);
      setNotice(undefined);
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
        // The card overlay sits above the agent's pane: close it, or the pane
        // opens hidden behind it (PRD #2374 review).
        setOpen(undefined);
        openAgent({ deckId: card.deckId, agentId: card.entry.agent_id, ...(card.entry.instance_id ? { incarnation: card.entry.instance_id } : {}) });
        return undefined;
      }
      if (action.text_input) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "reply" } });
        return undefined;
      }
      // A confirm action, a close, and any press from a control that showed
      // only the action's label stop at a step showing exactly what it does.
      if (action.confirm || wanted.review) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "confirm", actionId: action.id, token: action.token } });
        return undefined;
      }
      // A control that showed the text names the definition it showed (its
      // token). Without one — voice names an action by its label — text the
      // agent wrote is shown at the review step and sent only from there.
      if (wanted.token === undefined && !sendableUnread(action)) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "confirm", actionId: action.id, token: action.token } });
        return agentWrittenReview(displayText(action.label, DISPLAY_LIMITS.name), name);
      }
      send(card, action, wanted.token !== undefined ? { token: wanted.token } : {});
      return undefined;
    },
    replyToAgentCard: (ref, text, sendNow) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const reply = card.entry.actions.find((candidate) => candidate.id === "reply");
      if (!reply) return `${displayText(card.entry.name, DISPLAY_LIMITS.name)}'s card takes no reply.`;
      if (!reply.enabled) return `Reply is disabled: ${disabledReasonSentence(reply.disabled_reason)}.`;
      // Spoken words are put in the card's reply box for the user to read,
      // edit and send, as `dictate_to_agent` types into a prompt without
      // sending it; only the Send reply button, which shows them, sends.
      if (!sendNow) {
        setRowMenu(undefined);
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "reply", text } });
        return undefined;
      }
      send(card, reply, { text, token: reply.token });
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
      // The definition reviewed is what is confirmed: a changed action is
      // shown again for review, never sent on the old approval.
      if (action.token !== current.step.token) return;
      send(card, action, { confirm: action.confirm, token: current.step.token });
      setOpen({ key: current.key, name: current.name });
    },
    toggleOverviewRowMenu: (rowKey) => setRowMenu((current) => (current === rowKey ? undefined : rowKey)),
    setCardTree: (ref, wantOpen) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const name = displayText(card.entry.name, DISPLAY_LIMITS.name);
      const { placed: listed, treeFor: tree, hiddenByFilter: hidden } = latest.current;
      const sameDaemon = (other: PlacedCard) => other.deckId === card.deckId && other.entry.instance_id === card.entry.instance_id;
      const children = tree(card.deckId) ? listed.filter((other) => sameDaemon(other) && other.entry.parent === card.entry.key && other.entry.key !== card.entry.key) : [];
      if (children.length === 0) return nothingUnder(name);
      const cardKey = placedCardKey(card);
      /* Opening a tree opens the way to it as well: every card above it, so
         the named card and what is under it are on screen. */
      const above = wantOpen ? pathTo(cardTree(listed.filter(sameDaemon), true), (node) => placedCardKey(node.card) === cardKey).open : new Set<string>();
      changeOpenCards((current) => {
        const want = [...above, cardKey];
        if (wantOpen ? want.every((wanted) => current.has(wanted)) : !current.has(cardKey)) return current;
        const next = new Set(current);
        if (wantOpen) want.forEach((wanted) => next.add(wanted));
        else next.delete(cardKey);
        return next;
      });
      // The tree is the Cards view's: asking for it shows that view.
      remember("cards");
      // Open but kept off screen by the dashboard filter: say so rather than report it done.
      if (wantOpen && hidden?.(card)) return treeHiddenByFilter(name);
      if (wantOpen && hidden && children.every(hidden)) return treeEmptiedByFilter(name);
      return undefined;
    },
    openCardReference: (ref, kind, url) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const name = displayText(card.entry.name, DISPLAY_LIMITS.name);
      const what = kind === "pull_request" ? "pull request" : "issue";
      const refs = (latest.current.treeFor(card.deckId) ? card.entry.checked_refs ?? [] : []).filter((candidate) => candidate.kind === kind && (url === undefined || candidate.url === url));
      if (refs.length === 0) return `${name}'s card names no ${what}.`;
      const checked = refs.find(openableReference);
      if (!checked) {
        const first = refs[0];
        const why = first.result.status === "pending" ? "the deck has not checked it yet" : "the deck could not check it";
        return `${name}'s ${what} ${displayText(first.claim, DISPLAY_LIMITS.name)} does not open: ${why}.`;
      }
      const opener = latest.current.openReference;
      if (!opener) return NO_REFERENCE_OPENER;
      /* Named by the card, so the card of an agent that stopped opens its
         references too: the shell checks it against the card its deck lists. */
      const refused = opener({ deckId: card.deckId, instanceId: card.entry.instance_id, cardKey: card.entry.key, kind, number: checked.number, url: checked.url });
      if (refused !== undefined) return refused;
      /* The page opens over the dashboard, and the card overlay is drawn
         over that and would fence it: the overlay closes so the page is in
         front, as a card's Open closes it for the pane (issue #2418 review,
         F1). */
      setOpen(undefined);
      setNotice(undefined);
      return undefined;
    },
  }), [changeOpenCards, find, openAgent, remember, send]);

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
      tree: at >= 0 && treeFor(placed[at].deckId),
    };
  }, [open, placed, results, shown, treeFor]);

  const overlayOpen = overlay !== undefined;
  const voice = useMemo<Partial<AgentCardControls>>(() => (
    overlayOpen ? controls : { ...controls, closeAgentCard: undefined }
  ), [controls, overlayOpen]);

  const clickControls = useMemo(() => ({ ...controls, reportRefused: setNotice }), [controls]);
  const dismissNotice = useCallback(() => setNotice(undefined), []);

  const resultsFor = useCallback((card: PlacedCard) => results[placedCardKey(card)] ?? {}, [results]);
  return { shown, mode, needsYouOnly, placed, decksWithCards, openCards, treeFor, overlay, notice, dismissNotice, rowMenu, controls, clickControls, voice, cardFor, resultsFor };
}
