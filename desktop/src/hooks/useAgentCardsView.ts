import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { actionBySpokenLabel, cardTarget, disabledReasonSentence, placeCards, placedCardKey, pruneCardResults, readOverviewMode, sendableUnread, stepCard, writeOverviewMode, type CardOverlayStep, type CardResults, type CardStep, type OverviewMode, type PlacedCard } from "../lib/agentCards";
import { boundOpenCards, cardTree, openableReference, pathTo, pruneOpenCards, readOpenCards, writeOpenCards } from "../lib/agentCardsTree";
import { newCardRequestId } from "../lib/bridge";
import type { CardReferenceTarget } from "../lib/prBrowser";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { AgentCardRef, VoiceActionContext } from "../lib/voiceActions";
import type { AgentCardAction, CardBatchOutcome, DeckRuntimeState } from "../types";

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

/** …when a batch is asked of a daemon that does not do batches. */
const noBatches = (name: string) => `${name}'s daemon cannot send an action to several agents at once; upgrade it to use batches.`;
/** …when this app has no way to send a batch. */
const NO_BATCH_BRIDGE = "This window cannot send an action to several agents at once.";
/** …when a selection batch is asked for with nothing selected. */
const NOTHING_SELECTED = "No card is selected.";

/** The card members the overview serves, every one of them. */
type AgentCardControls = Pick<VoiceActionContext, "setOverviewMode" | "showAgentCard" | "stepAgentCard" | "setNeedsYouFilter" | "closeAgentCard" | "runCardAction" | "replyToAgentCard" | "confirmCardAction" | "toggleOverviewRowMenu" | "setCardTree" | "openCardReference" | "runCardActionBatch" | "toggleCardSelecting" | "toggleCardSelected" | "runSelectionBatch" | "sendCardBatch" | "closeCardBatch">;

/** Issue #2399 — an action a batch can send: its id and the label its first card shows. */
export interface CardBatchChoice {
  id: string;
  label: string;
}

/** Issue #2399 — one daemon's part of a batch: its preview's or its execution's answer per card. */
export interface CardBatchGroup {
  deckId: string;
  instanceId: string;
  entries: CardBatchOutcome[];
}

/**
 * Issue #2399 — the batch on screen: asking the daemons for a preview, the
 * confirmation built from it, sending, and the result per agent. Everything
 * it lists is what the daemons answered; nothing is decided here about who is
 * included or what text they get.
 */
export interface CardBatchView {
  actionId: string;
  label: string;
  /** What the batch reaches, in words: "root and the 2 agents under it", "3 selected agents". */
  scope: string;
  stage: "previewing" | "review" | "sending" | "done";
  /** The preview's answer, per daemon. */
  groups: CardBatchGroup[];
  /** The execution's answer, per daemon (stage `done`). */
  results: CardBatchGroup[];
  /** Why the preview could not be read. */
  error?: string;
}

/** Issue #2399 — the Cards view's selection: whether it is on, and the cards in it by placed key. */
export interface CardSelection {
  selecting: boolean;
  selected: ReadonlySet<string>;
  /** The actions the selected cards offer a batch, in the order they are first met. */
  choices: CardBatchChoice[];
}

/**
 * Issue #2399 — the actions a batch can send to `cards`: every action one of
 * them offers, but an Open (the app does it), a reply (the user's own words,
 * one card at a time) and an answer to one agent's own question. Whether each
 * card gets it is the daemon's call, in the preview.
 */
export function batchChoices(cards: readonly PlacedCard[]): CardBatchChoice[] {
  const choices = new Map<string, string>();
  for (const card of cards) {
    const options = new Set(card.entry.questions.flatMap((question) => question.options.map((option) => option.action_id)));
    for (const action of card.entry.actions) {
      if (action.kind === "open" || action.text_input || options.has(action.id) || choices.has(action.id)) continue;
      choices.set(action.id, action.label);
    }
  }
  return [...choices].map(([id, label]) => ({ id, label }));
}

/**
 * Issue #2399 — `card` and every card its daemon hung under it, at any depth,
 * in the daemon's order: what a batch from that card reaches, and what
 * selecting it while it is closed selects.
 */
export function cardSubtree(listed: readonly PlacedCard[], card: PlacedCard, tree: boolean): PlacedCard[] {
  const sameDaemon = listed.filter((other) => other.deckId === card.deckId && other.entry.instance_id === card.entry.instance_id);
  const wanted = placedCardKey(card);
  const found = (nodes: ReturnType<typeof cardTree>): ReturnType<typeof cardTree>[number] | undefined => {
    for (const node of nodes) {
      if (placedCardKey(node.card) === wanted) return node;
      const below = found(node.children);
      if (below) return below;
    }
    return undefined;
  };
  const node = found(cardTree(sameDaemon, tree));
  if (!node) return [card];
  const keys = new Set<string>();
  const walk = (at: typeof node) => { keys.add(placedCardKey(at.card)); at.children.forEach(walk); };
  walk(node);
  return sameDaemon.filter((other) => keys.has(placedCardKey(other)));
}

/** What a batch reaching `cards` from `top` is called on screen. */
function subtreeScope(top: PlacedCard, cards: readonly PlacedCard[]): string {
  const name = displayText(top.entry.name, DISPLAY_LIMITS.name);
  const under = cards.length - 1;
  return under === 0 ? name : `${name} and the ${under} ${under === 1 ? "agent" : "agents"} under it`;
}

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
  /** Issue #2399 — the batch actions for the open card and every card under it, when its daemon does batches and it has cards under it. */
  batch?: { choices: CardBatchChoice[]; under: number };
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
  /** Issue #2399 — whether a deck's daemon does batch actions. */
  batchFor: (deckId: string) => boolean;
  /** Issue #2399 — the selection, while any deck on screen does batches. */
  selection: CardSelection | undefined;
  /** Issue #2399 — the batch on screen, from its preview to its results. */
  batch: CardBatchView | undefined;
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
  runtime: Pick<DeckRuntimeState, "agentCards" | "invokeCardAction" | "invokeCardActionBatch">;
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
  /* Issue #2399 — the selection and the batch on screen. */
  const batchFor = useCallback((deckId: string) => runtime.agentCards?.[deckId]?.batch === true, [runtime.agentCards]);
  const [selecting, setSelecting] = useState(false);
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set());
  const [batch, setBatch] = useState<CardBatchView>();
  /* Bumped by every batch started or closed, so a late answer to an earlier one changes nothing. */
  const batchRun = useRef(0);
  /* The latest committed state, for members a dispatch calls after a render. */
  const latest = useRef({ placed, open, shown, treeFor, openReference, hiddenByFilter, openCards, selecting, selected, batch, batchFor });
  latest.current = { placed, open, shown, treeFor, openReference, hiddenByFilter, openCards, selecting, selected, batch, batchFor };
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
  /* A selected card its daemon no longer lists leaves the selection. */
  useEffect(() => {
    setSelected((current) => {
      if (current.size === 0) return current;
      const listed = new Set(placed.map(placedCardKey));
      const kept = [...current].filter((cardKey) => listed.has(cardKey));
      return kept.length === current.size ? current : new Set(kept);
    });
  }, [placed]);
  const invoke = runtime.invokeCardAction;
  const invokeBatch = runtime.invokeCardActionBatch;
  /* Issue #2399 — ask each daemon for its preview of `actionId` on its share
     of `cards` and open the confirmation; it shows what the daemons answered
     and sends nothing. */
  const startBatch = useCallback((cards: readonly PlacedCard[], actionId: string, label: string, scope: string): string | undefined => {
    if (!invokeBatch) return NO_BATCH_BRIDGE;
    const groups = new Map<string, { deckId: string; instanceId: string; cards: PlacedCard[] }>();
    for (const card of cards) {
      const groupKey = `${card.deckId}\u0000${card.entry.instance_id}`;
      const group = groups.get(groupKey) ?? { deckId: card.deckId, instanceId: card.entry.instance_id, cards: [] };
      group.cards.push(card);
      groups.set(groupKey, group);
    }
    const run = ++batchRun.current;
    setRowMenu(undefined);
    setOpen(undefined);
    setNotice(undefined);
    setBatch({ actionId, label, scope, stage: "previewing", groups: [], results: [] });
    void Promise.all([...groups.values()].map(async (group): Promise<CardBatchGroup> => {
      const answer = await invokeBatch(group.deckId, actionId, {
        mode: "preview",
        entries: group.cards.map((card) => ({ pane_id: card.entry.pane_id, agent_id: card.entry.agent_id })),
        requestId: newCardRequestId(),
        instanceId: group.instanceId,
      });
      return { deckId: group.deckId, instanceId: group.instanceId, entries: answer.entries };
    })).then(
      (answered) => { if (batchRun.current === run) setBatch((current) => current && { ...current, stage: "review", groups: answered }); },
      (cause: unknown) => { if (batchRun.current === run) setBatch((current) => current && { ...current, stage: "review", error: cause instanceof Error ? cause.message : String(cause) }); },
    );
    return undefined;
  }, [invokeBatch]);
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
      // Text the catalog did not write (the agent's, or an answer quoting its
      // question) is shown whole at the review step and sent only from there,
      // whoever pressed (audit A4; issue #2418 round 2, R2-A1). A control's
      // token names the definition it drew, not that its text was on screen:
      // the overlay draws a prompt under its button, where the end of a long
      // one can lie below the overlay's visible part. So the token is never a
      // reason to skip this step; only the prompt's source is.
      if (!sendableUnread(action)) {
        setOpen({ key: placedCardKey(card), name: card.entry.name, step: { kind: "confirm", actionId: action.id, token: action.token } });
        // A button press sees the step open; voice is told why it did.
        return wanted.token === undefined ? agentWrittenReview(displayText(action.label, DISPLAY_LIMITS.name), name) : undefined;
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
    runCardActionBatch: (ref, wanted) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const name = displayText(card.entry.name, DISPLAY_LIMITS.name);
      if (!latest.current.batchFor(card.deckId)) return noBatches(name);
      const cards = cardSubtree(latest.current.placed, card, latest.current.treeFor(card.deckId));
      const choices = batchChoices(cards);
      let choice = wanted.actionId !== undefined ? choices.find((candidate) => candidate.id === wanted.actionId) : undefined;
      if (wanted.actionId === undefined && wanted.label !== undefined) {
        for (const each of cards) {
          const spoken = actionBySpokenLabel(each.entry, wanted.label);
          choice = spoken ? choices.find((candidate) => candidate.id === spoken.id) : undefined;
          if (choice) break;
        }
      }
      if (!choice) return wanted.label !== undefined ? `Neither ${name} nor any card under it has an action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.` : CARD_GONE;
      return startBatch(cards, choice.id, choice.label, subtreeScope(card, cards));
    },
    toggleCardSelecting: () => {
      const on = !latest.current.selecting;
      setSelecting(on);
      // Leaving the selection clears it.
      if (!on) setSelected(new Set());
    },
    toggleCardSelected: (ref) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      if (!latest.current.batchFor(card.deckId)) return noBatches(displayText(card.entry.name, DISPLAY_LIMITS.name));
      const cardKey = placedCardKey(card);
      /* A closed card stands for everything under it, so it takes its whole tree along; an open one is itself. */
      const closed = !latest.current.openCards.has(cardKey);
      const keys = (closed ? cardSubtree(latest.current.placed, card, latest.current.treeFor(card.deckId)) : [card]).map(placedCardKey);
      setSelecting(true);
      setSelected((current) => {
        const next = new Set(current);
        if (current.has(cardKey)) keys.forEach((each) => next.delete(each));
        else keys.forEach((each) => next.add(each));
        return next;
      });
      return undefined;
    },
    runSelectionBatch: (actionId) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const { placed: listed, selected: chosen, batchFor: does } = latest.current;
      const cards = listed.filter((card) => chosen.has(placedCardKey(card)) && does(card.deckId));
      if (cards.length === 0) return NOTHING_SELECTED;
      const choice = batchChoices(cards).find((candidate) => candidate.id === actionId);
      if (!choice) return "None of the selected cards offers that action any more, so nothing was sent.";
      return startBatch(cards, choice.id, choice.label, `${cards.length} selected ${cards.length === 1 ? "agent" : "agents"}`);
    },
    sendCardBatch: () => {
      const current = latest.current.batch;
      if (!current || current.stage !== "review" || !invokeBatch) return;
      const run = ++batchRun.current;
      const confirmed = current.groups
        .map((group) => ({ group, included: group.entries.filter((entry) => entry.outcome === "included") }))
        .filter(({ included }) => included.length > 0);
      if (confirmed.length === 0) return;
      setBatch({ ...current, stage: "sending" });
      void Promise.all(confirmed.map(async ({ group, included }): Promise<CardBatchGroup> => {
        try {
          const answer = await invokeBatch(group.deckId, current.actionId, {
            mode: "execute",
            entries: included.map((entry) => ({ pane_id: entry.pane_id, agent_id: entry.agent_id, action_token: entry.action_token ?? "", instance_id: entry.instance_id ?? "" })),
            requestId: newCardRequestId(),
            instanceId: group.instanceId,
          });
          return { deckId: group.deckId, instanceId: group.instanceId, entries: answer.entries };
        } catch (cause) {
          const message = cause instanceof Error ? cause.message : String(cause);
          return { deckId: group.deckId, instanceId: group.instanceId, entries: included.map((entry) => ({ pane_id: entry.pane_id, agent_id: entry.agent_id, outcome: "failed", message })) };
        }
      })).then((results) => { if (batchRun.current === run) setBatch((shown) => shown && { ...shown, stage: "done", results }); });
    },
    closeCardBatch: () => {
      batchRun.current += 1;
      setBatch(undefined);
    },
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
  }), [changeOpenCards, find, invokeBatch, openAgent, remember, send, startBatch]);

  const overlay = useMemo<CardOverlayView | undefined>(() => {
    if (!open || !shown) return undefined;
    const at = placed.findIndex((card) => placedCardKey(card) === open.key);
    const card = at < 0 ? undefined : placed[at];
    const under = card && batchFor(card.deckId) ? cardSubtree(placed, card, treeFor(card.deckId)) : [];
    return {
      card,
      lastName: open.name,
      position: at + 1,
      count: placed.length,
      step: open.step,
      results: results[open.key] ?? {},
      tree: at >= 0 && treeFor(placed[at].deckId),
      ...(under.length > 1 ? { batch: { choices: batchChoices(under), under: under.length - 1 } } : {}),
    };
  }, [batchFor, open, placed, results, shown, treeFor]);

  const overlayOpen = overlay !== undefined;
  const voice = useMemo<Partial<AgentCardControls>>(() => (
    overlayOpen ? controls : { ...controls, closeAgentCard: undefined }
  ), [controls, overlayOpen]);

  const clickControls = useMemo(() => ({ ...controls, reportRefused: setNotice }), [controls]);
  const dismissNotice = useCallback(() => setNotice(undefined), []);

  const resultsFor = useCallback((card: PlacedCard) => results[placedCardKey(card)] ?? {}, [results]);
  /* Issue #2399 — the selection is offered while any deck on screen does batches. */
  const anyBatches = shown && placed.some((card) => batchFor(card.deckId));
  const selection = useMemo<CardSelection | undefined>(() => {
    if (!anyBatches) return undefined;
    return { selecting, selected, choices: batchChoices(placed.filter((card) => selected.has(placedCardKey(card)))) };
  }, [anyBatches, placed, selected, selecting]);
  return { shown, mode, needsYouOnly, placed, decksWithCards, openCards, treeFor, overlay, notice, dismissNotice, rowMenu, controls, clickControls, voice, cardFor, resultsFor, batchFor, selection, batch: shown ? batch : undefined };
}
