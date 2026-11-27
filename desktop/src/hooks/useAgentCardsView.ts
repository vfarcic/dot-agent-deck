import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { actionLabel, cardTarget, spokenLabelMatch, disabledReasonSentence, placeCards, placedCardKey, pruneCardResults, readOverviewMode, sendableUnread, stepCard, writeOverviewMode, type CardResults, type CardStep, type OverviewMode, type PlacedCard } from "../lib/agentCards";
import { boundOpenCards, cardTree, openableReference, pathTo, pruneOpenCards, readOpenCards, writeOpenCards } from "../lib/agentCardsTree";
import { newCardRequestId } from "../lib/bridge";
import type { CardReferenceTarget } from "../lib/prBrowser";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { AgentCardRef, VoiceActionContext } from "../lib/voiceActions";
import type { AgentCardAction, CardBatchOutcome, CardTabSection, DeckRuntimeState } from "../types";

/** What a card member says when the cards are not shown (this connection offers no cards). */
const CARDS_HIDDEN = "Agent cards are not available here.";
/** …when the card it names is not on the dashboard any more. */
const CARD_GONE = "That agent has no card on the dashboard any more, so nothing was done.";
/** …when the single-action review is asked to close with none open. */
const NO_REVIEW_OPEN = "No action is waiting for you to review it.";
/** …when no agent needs the user. */
const NO_CARD_NEEDS_YOU = "No other agent needs you right now.";
/** …when there are no agents with cards to move between. */
const NO_CARDS = "There are no agents with cards to move between.";
/** …when voice named an action whose text the agent wrote: it opens for review instead of sending. */
const agentWrittenReview = (label: string, name: string) => `${name} wrote the text of ${label}, so it is shown for you to read first: press Send to send it.`;

/** …when a spoken label names more than one of a card's buttons (PR #2491 review J3, audit K2). */
const ambiguousLabel = (name: string, label: string) => `${name}'s card has more than one action called "${displayText(label, DISPLAY_LIMITS.name)}", so nothing was sent.`;

/**
 * The batch action a spoken label names: the first card, in order, whose
 * buttons it names gives it; a card on which it names more than one button
 * makes it `ambiguous`, refused rather than read as another card's.
 */
function spokenChoice(cards: readonly PlacedCard[], choices: readonly CardBatchChoice[], label: string): CardBatchChoice | "ambiguous" | undefined {
  for (const each of cards) {
    const spoken = spokenLabelMatch(each.entry, label);
    if (spoken.outcome === "ambiguous") return "ambiguous";
    const choice = spoken.outcome === "found" ? choices.find((candidate) => candidate.id === spoken.action.id) : undefined;
    if (choice) return choice;
  }
  return undefined;
}

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
/** …when the batch on screen is asked to close while it is being sent: its results would be lost while the daemons still act. */
export const BATCH_STILL_SENDING = "The batch is still being sent, so it stays open: the result for each agent shows here when every daemon has answered.";
/** What follows the error on each agent's row when a batch's execution failed in a way the desktop could not place before or after the request left. */
export const BATCH_NOT_KNOWN = "it is not known whether this agent got it. Check the agent before sending again";
/**
 * Why an agent is skipped when its daemon could not be reached or did not
 * answer the preview (PR #2491 review): the other daemons' agents still
 * proceed, and nothing is sent to this one. The error follows. A daemon that
 * answered by refusing the preview is not this: the desktop bridge answers
 * each of its agents skipped with the daemon's own reason.
 */
export const BATCH_DAEMON_UNREACHABLE = "its daemon could not be reached or did not answer the preview, so nothing is sent to it";
/** The code such a skipped agent carries. */
const CODE_BATCH_DAEMON_UNREACHABLE = "daemon_unreachable";
/** …when another batch is asked for while one is being sent. */
export const ANOTHER_BATCH_SENDING = "A batch is still being sent; wait for its results before starting another.";

/** The card members the overview serves, every one of them. */
type AgentCardControls = Pick<VoiceActionContext, "setOverviewMode" | "showAgentCard" | "stepAgentCard" | "setNeedsYouFilter" | "closeCardReview" | "runCardAction" | "confirmCardAction" | "toggleOverviewRowMenu" | "setCardTree" | "openCardReference" | "runCardActionBatch" | "clearCardSelection" | "toggleCardSelected" | "runSelectionBatch" | "sendCardBatch" | "closeCardBatch">;

/**
 * Issue #2399 — an action a batch can send: its id and the label and icon its
 * first card shows, and (decision 2026-11-24) the exact text it sends when
 * every card offering it sends the same one, or `differs` when they do not.
 * The text is for the button's hover only: what each agent gets is the
 * daemon's preview, in the confirmation.
 */
interface CardBatchChoice {
  id: string;
  label: string;
  icon?: string | null;
  kind?: string;
  text?: string;
  differs?: boolean;
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
  /** Why the last attempt to close it was refused (it was being sent), shown in the dialog. */
  refusal?: string;
}

/** Issue #2399 — the Cards view's selection: the cards in it by placed key (PR #2491 review: every card's checkbox is always shown, so there is no selecting mode). */
export interface CardSelection {
  selected: ReadonlySet<string>;
  /** PR #2491 review H1 — the card whose checkbox was ticked or unticked last, by placed key: where focus goes back to when the bar takes its own away. */
  last?: string;
  /** The actions the selected cards offer a batch, in the order they are first met. */
  choices: CardBatchChoice[];
  /** PR #2491 (2026-11-25) — the daemons the selected cards are on, each once, in the dashboard's order: the selection bar's Edit card actions opens one of their editors. */
  deckIds: string[];
}

/**
 * Issue #2399 — the actions a batch can send to `cards`: every action one of
 * them offers, but an Open (the app does it), a reply (the user's own words,
 * one card at a time) and an answer to one agent's own question. Whether each
 * card gets it is the daemon's call, in the preview.
 */
function batchChoices(cards: readonly PlacedCard[]): CardBatchChoice[] {
  const choices = new Map<string, CardBatchChoice & { texts: Set<string> }>();
  for (const card of cards) {
    const options = new Set(card.entry.questions.flatMap((question) => question.options.map((option) => option.action_id)));
    for (const action of card.entry.actions) {
      if (action.kind === "open" || action.text_input || options.has(action.id)) continue;
      const choice = choices.get(action.id) ?? { id: action.id, label: action.label, icon: action.icon, kind: action.kind, texts: new Set<string>() };
      if (action.prompt) choice.texts.add(action.prompt);
      choices.set(action.id, choice);
    }
  }
  return [...choices.values()].map(({ texts, ...choice }) => ({
    ...choice,
    ...(texts.size === 1 ? { text: [...texts][0] } : texts.size > 1 ? { differs: true } : {}),
  }));
}

/**
 * Issue #2399 — `card` and every card its daemon hung under it, at any depth,
 * in the daemon's order: what a batch from that card reaches, and what
 * selecting it while it is closed selects.
 */
function cardSubtree(listed: readonly PlacedCard[], card: PlacedCard, tree: boolean): PlacedCard[] {
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

/**
 * PR #2491 decision 2026-11-25 — the single-action review on screen: the card
 * and the version (`token`) of the action it shows, and whether Send was
 * pressed, after which it shows what became of it.
 */
export interface CardReviewView {
  /** The card, `undefined` once its agent is gone. */
  card: PlacedCard | undefined;
  /** The card's name when the review opened, for the closed notice. */
  lastName: string;
  actionId: string;
  token: string;
  sent: boolean;
  results: CardResults;
  /**
   * PR #2491 review J4 — once Send is pressed, the action as it was reviewed
   * and sent, its label, and what became of the press, kept apart from the
   * card: a Close removes the card, and an agent can stop offering an action
   * once it has it, and neither makes the review say nothing was sent.
   */
  reviewed?: { action: AgentCardAction; label: string; result: CardResults[string] };
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
  /** PR #2491 decision 2026-11-25 — the single-action review on screen. */
  review: CardReviewView | undefined;
  /**
   * Issue #2418 review (F2) — why the last thing pressed on a card or in its
   * Card tab did not happen, until it is dismissed, the review closes, or a
   * reference opens: a press that did nothing says why.
   */
  notice: string | undefined;
  dismissNotice: () => void;
  /** The table row whose ⋯ menu is open. */
  rowMenu: string | undefined;
  controls: AgentCardControls;
  /**
   * {@link controls} for the cards' and the Card tab's own controls, which
   * report a refusal in {@link notice}. Kept apart from `controls`, which is
   * also published to voice, where the voice panel reports refusals itself.
   */
  clickControls: AgentCardControls & Pick<VoiceActionContext, "reportRefused">;
  /** The members published to voice: `closeCardReview` only while a review is open. */
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

/**
 * PR #2491 review — the selected cards a selection batch can reach: those
 * still listed whose daemon advertises batches. The bar's count and actions,
 * a batch started from it and the pruning of the selection all read this one
 * rule, so a card the bar counts is a card an action from it reaches.
 */
function selectedBatchCards(placed: readonly PlacedCard[], selected: ReadonlySet<string>, batchFor: (deckId: string) => boolean): PlacedCard[] {
  if (selected.size === 0) return [];
  return placed.filter((card) => selected.has(placedCardKey(card)) && batchFor(card.deckId));
}

function sameIncarnation(card: PlacedCard, incarnation: string | undefined): boolean {
  return incarnation === undefined || card.entry.instance_id === "" || card.entry.instance_id === incarnation;
}

/**
 * PRD #2374 — the dashboard's cards view: which view is on screen, the Needs
 * you filter, the single-action review, each table row's menu, and the card
 * members every control and voice dispatch through. It renders and sends what
 * the daemon decided; the only things decided here are the user's own choices
 * (view, filter, which review is open). A card itself is shown in its agent's
 * pane (PR #2491 decision 2026-11-25), which `openAgent` opens.
 */
export function useAgentCardsView({ runtime, decks, openAgent, pane, openReference, hiddenByFilter }: {
  runtime: Pick<DeckRuntimeState, "agentCards" | "invokeCardAction" | "invokeCardActionBatch">;
  decks: readonly { deckId: string | undefined; connected: boolean }[];
  /** Open a card's agent's pane — on Terminal, or with `cardTab` on its Card tab (where the user asked for the card itself). */
  openAgent: (card: PlacedCard, options?: { cardTab?: boolean; cardSection?: CardTabSection }) => void;
  /** The agent whose pane is open over the dashboard, if any: where the header's previous and next move from. */
  pane?: { deckId: string; agentId: string; incarnation?: string };
  /** Issue #2418 — open a checked pull request or issue in the app's GitHub browser; absent where there is none. */
  openReference?: (target: CardReferenceTarget) => string | undefined;
  /** Issue #2418 — whether the dashboard filter keeps a card off screen; absent when nothing filters. */
  hiddenByFilter?: (card: PlacedCard) => boolean;
}): AgentCardsView {
  const shown = runtime.agentCards !== undefined;
  const [storedMode, setStoredMode] = useState<OverviewMode>(readOverviewMode);
  const mode: OverviewMode = shown ? storedMode : "table";
  const [needsYouOnly, setNeedsYouOnly] = useState(false);
  const [review, setReview] = useState<{ key: string; name: string; actionId: string; token: string; sent?: boolean; reviewed?: CardReviewView["reviewed"]; run?: number }>();
  /* Numbers each Send from a review, so a late answer lands only on the review that sent it. */
  const reviewRun = useRef(0);
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
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set());
  const [lastToggled, setLastToggled] = useState<string>();
  const [batch, setBatch] = useState<CardBatchView>();
  /* Bumped by every batch started or closed, so a late answer to an earlier one changes nothing. */
  const batchRun = useRef(0);
  /* Whether a batch is being sent, set in the same tick as Send so a close or
     a new batch asked for before the next render is refused too. */
  const batchSending = useRef(false);
  /* The latest committed state, for members a dispatch calls after a render. */
  const latest = useRef({ placed, review, pane, shown, treeFor, openReference, hiddenByFilter, openCards, selected, batch, batchFor });
  latest.current = { placed, review, pane, shown, treeFor, openReference, hiddenByFilter, openCards, selected, batch, batchFor };
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
  /* A selected card its daemon no longer lists, or whose daemon stopped
     advertising batches (a downgrade, a reconnect to an older daemon), leaves
     the selection: it has no checkbox and no batch reaches it, so it is not
     kept hidden in the selection either, to come back ticked later. */
  useEffect(() => {
    setSelected((current) => {
      if (current.size === 0) return current;
      const kept = selectedBatchCards(placed, current, batchFor).map(placedCardKey);
      return kept.length === current.size ? current : new Set(kept);
    });
  }, [batchFor, placed]);
  const invoke = runtime.invokeCardAction;
  const invokeBatch = runtime.invokeCardActionBatch;
  /* Issue #2399 — ask each daemon for its preview of `actionId` on its share
     of `cards` and open the confirmation; it shows what the daemons answered
     and sends nothing. */
  const startBatch = useCallback((cards: readonly PlacedCard[], actionId: string, label: string, scope: string): string | undefined => {
    if (!invokeBatch) return NO_BATCH_BRIDGE;
    /* A batch being sent keeps the screen until its results are in: a new one
       would replace it and lose what each agent got. */
    if (batchSending.current) return ANOTHER_BATCH_SENDING;
    const groups = new Map<string, { deckId: string; instanceId: string; cards: PlacedCard[] }>();
    for (const card of cards) {
      const groupKey = `${card.deckId}\u0000${card.entry.instance_id}`;
      const group = groups.get(groupKey) ?? { deckId: card.deckId, instanceId: card.entry.instance_id, cards: [] };
      group.cards.push(card);
      groups.set(groupKey, group);
    }
    const run = ++batchRun.current;
    setRowMenu(undefined);
    setReview(undefined);
    setNotice(undefined);
    setBatch({ actionId, label, scope, stage: "previewing", groups: [], results: [] });
    /* Each daemon's preview settles on its own (PR #2491 review): one that
       cannot be reached, is not trusted or does not answer leaves its agents
       skipped with its error, and the other daemons' agents proceed. One that
       answered and declined the preview (its handshake classified
       incompatible, a refused `Hello`, a replaced or older daemon, the
       request refused whole) has it answered by the bridge, each agent
       skipped with the reason, so it is not read as unanswered. Send sends only to agents a preview
       included, so none of them is sent to. When no daemon answered, there
       is nothing to review, and the error is the batch's. */
    const parts = [...groups.values()];
    const reason = (cause: unknown) => (cause instanceof Error ? cause.message : String(cause));
    void Promise.allSettled(parts.map(async (group): Promise<CardBatchGroup> => {
      const answer = await invokeBatch(group.deckId, actionId, {
        mode: "preview",
        entries: group.cards.map((card) => ({ pane_id: card.entry.pane_id, agent_id: card.entry.agent_id })),
        requestId: newCardRequestId(),
        instanceId: group.instanceId,
      });
      /* An entry the daemon answered without a name (a preview it refused
         whole, which the bridge answers for each agent) keeps the name its
         card shows. */
      const named = new Map(group.cards.map((card) => [`${card.entry.pane_id}\u0000${card.entry.agent_id}`, card.entry.name]));
      const entries = answer.entries.map((entry) => entry.name !== undefined ? entry : { ...entry, name: named.get(`${entry.pane_id}\u0000${entry.agent_id}`) });
      return { deckId: group.deckId, instanceId: group.instanceId, entries };
    })).then((settled) => {
      if (batchRun.current !== run) return;
      const failures = settled.flatMap((part) => (part.status === "rejected" ? [reason(part.reason)] : []));
      if (failures.length === settled.length) {
        const error = [...new Set(failures)].join("; ");
        setBatch((current) => current && { ...current, stage: "review", error });
        return;
      }
      const answered = settled.map((part, at): CardBatchGroup => {
        if (part.status === "fulfilled") return part.value;
        const group = parts[at];
        const message = `${BATCH_DAEMON_UNREACHABLE}: ${reason(part.reason)}`;
        return {
          deckId: group.deckId,
          instanceId: group.instanceId,
          entries: group.cards.map((card) => ({ pane_id: card.entry.pane_id, agent_id: card.entry.agent_id, name: card.entry.name, outcome: "skipped", code: CODE_BATCH_DAEMON_UNREACHABLE, message })),
        };
      });
      setBatch((current) => current && { ...current, stage: "review", groups: answered });
    });
    return undefined;
  }, [invokeBatch]);
  /* `token` is the version of the action the user was shown — the pressed
     button's, or the reviewed step's — and the daemon sends nothing for any
     other (review B2). */
  const send = useCallback((card: PlacedCard, action: AgentCardAction, options: { confirm?: boolean; text?: string; token?: string }, onAnswer?: (answer: CardResults[string]) => void) => {
    const cardKey = placedCardKey(card);
    const actionToken = options.token ?? action.token;
    const record = (answer: CardResults[string]) => {
      setResults((current) => ({ ...current, [cardKey]: { ...current[cardKey], [actionToken]: answer } }));
      onAnswer?.(answer);
    };
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
    showAgentCard: (ref, opensOn) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      setRowMenu(undefined);
      /* PR #2491 decision 2026-11-26 — on Terminal unless the user asked for
         the card itself: a spoken "show its card", or a `+N` naming the part
         of the Card tab it counted. */
      openAgent(card, opensOn === "terminal" ? {} : { cardTab: true, ...(opensOn === "card" ? {} : { cardSection: opensOn }) });
      return undefined;
    },
    stepAgentCard: (step: CardStep) => {
      const { pane: current, placed: order } = latest.current;
      const from = current ? order.findIndex((card) => card.deckId === current.deckId && card.entry.agent_id === current.agentId && sameIncarnation(card, current.incarnation)) : -1;
      const at = stepCard(order, from, step);
      if (at === undefined) return step === "needs-you" ? NO_CARD_NEEDS_YOU : NO_CARDS;
      /* A move is a way into an agent, so its pane opens on Terminal. */
      openAgent(order[at]);
      return undefined;
    },
    setNeedsYouFilter: (on) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      /* Needs you only opens the path to every card that needs the user
         (the effect above, which also follows the cards while it is on). It
         applies to the Table view too (PR #2491 decision 2026-11-25), so
         turning it on keeps the view the user is on. */
      setNeedsYouOnly(on);
      return undefined;
    },
    closeCardReview: () => {
      if (!latest.current.review) return NO_REVIEW_OPEN;
      setReview(undefined);
      setNotice(undefined);
      return undefined;
    },
    runCardAction: (ref, wanted) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      const name = displayText(card.entry.name, DISPLAY_LIMITS.name);
      const spoken = wanted.actionId === undefined ? spokenLabelMatch(card.entry, wanted.label ?? "") : undefined;
      if (spoken?.outcome === "ambiguous") return ambiguousLabel(name, wanted.label ?? "");
      const action = wanted.actionId !== undefined
        ? card.entry.actions.find((candidate) => candidate.id === wanted.actionId)
        : spoken?.outcome === "found" ? spoken.action : undefined;
      if (!action) return wanted.label !== undefined ? `${name}'s card has no single action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.` : CARD_GONE;
      setRowMenu(undefined);
      if (!action.enabled) return `${displayText(action.label, DISPLAY_LIMITS.name)} is disabled: ${disabledReasonSentence(action.disabled_reason)}.`;
      /* Open is the pane itself, and a reply is the user's own words, typed
         into the agent's terminal there (PR #2491 decision 2026-11-25). */
      if (action.kind === "open" || action.text_input) {
        openAgent(card);
        return undefined;
      }
      const reviewIt = (sentence?: string) => {
        if (wanted.openPane) openAgent(card);
        setReview({ key: placedCardKey(card), name: card.entry.name, actionId: action.id, token: action.token });
        return sentence;
      };
      // A confirm action, a close, and any press from a control that showed
      // only the action's label stop at a review showing exactly what it does.
      if (action.confirm || wanted.review) return reviewIt();
      // Text the catalog did not write (the agent's, or an answer quoting its
      // question) is shown whole at the review and sent only from there,
      // whoever pressed (audit A4; issue #2418 round 2, R2-A1). A control's
      // token names the definition it drew, not that its text was on screen,
      // so the token is never a reason to skip this step; only the prompt's
      // source is.
      if (!sendableUnread(action)) {
        // A button press sees the review open; voice is told why it did.
        return reviewIt(wanted.token === undefined ? agentWrittenReview(displayText(action.label, DISPLAY_LIMITS.name), name) : undefined);
      }
      send(card, action, wanted.token !== undefined ? { token: wanted.token } : {});
      return undefined;
    },
    confirmCardAction: () => {
      const { review: current, placed: order } = latest.current;
      if (!current || current.sent) return;
      const card = order.find((candidate) => placedCardKey(candidate) === current.key);
      const actionId = current.actionId;
      const action = card?.entry.actions.find((candidate) => candidate.id === actionId);
      if (!card || !action || !action.enabled) return;
      // The definition reviewed is what is confirmed: a changed action is
      // shown again for review, never sent on the old approval.
      if (action.token !== current.token) return;
      /* What was sent and its answer stay with the review (J4), whatever
         becomes of the card or the action after. */
      const run = ++reviewRun.current;
      setReview({ ...current, sent: true, run, reviewed: { action, label: actionLabel(card.entry, action), result: { outcome: "pending" } } });
      send(card, action, { confirm: action.confirm, token: current.token }, (answer) => {
        setReview((shown) => (shown?.run === run && shown.reviewed ? { ...shown, reviewed: { ...shown.reviewed, result: answer } } : shown));
      });
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
        const spoken = spokenChoice(cards, choices, wanted.label);
        if (spoken === "ambiguous") return ambiguousLabel(name, wanted.label);
        choice = spoken;
      }
      if (!choice) return wanted.label !== undefined ? `Neither ${name} nor any card under it has an action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.` : CARD_GONE;
      return startBatch(cards, choice.id, choice.label, subtreeScope(card, cards));
    },
    clearCardSelection: () => {
      if (latest.current.selected.size === 0) return NOTHING_SELECTED;
      setSelected(new Set());
      return undefined;
    },
    toggleCardSelected: (ref, want) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const card = find(ref);
      if (!card) return CARD_GONE;
      if (!latest.current.batchFor(card.deckId)) return noBatches(displayText(card.entry.name, DISPLAY_LIMITS.name));
      const cardKey = placedCardKey(card);
      /* A closed card stands for everything under it, so it takes its whole tree along; an open one is itself. */
      const closed = !latest.current.openCards.has(cardKey);
      const keys = (closed ? cardSubtree(latest.current.placed, card, latest.current.treeFor(card.deckId)) : [card]).map(placedCardKey);
      /* The selection is the Cards view's: selecting by voice from the table shows that view. */
      remember("cards");
      setLastToggled(cardKey);
      setSelected((current) => {
        const next = new Set(current);
        /* A checkbox flips; voice says which it wants ("select", "unselect"), so saying it twice changes nothing. */
        if (want ?? !current.has(cardKey)) keys.forEach((each) => next.add(each));
        else keys.forEach((each) => next.delete(each));
        return next;
      });
      return undefined;
    },
    runSelectionBatch: (wanted) => {
      if (!latest.current.shown) return CARDS_HIDDEN;
      const { placed: listed, selected: chosen, batchFor: does } = latest.current;
      const cards = selectedBatchCards(listed, chosen, does);
      if (cards.length === 0) return NOTHING_SELECTED;
      const choices = batchChoices(cards);
      let choice = wanted.actionId !== undefined ? choices.find((candidate) => candidate.id === wanted.actionId) : undefined;
      /* A spoken label is matched the way a single card's is, against each selected card's own buttons. */
      if (wanted.actionId === undefined && wanted.label !== undefined) {
        const spoken = spokenChoice(cards, choices, wanted.label);
        if (spoken === "ambiguous") return `A selected card has more than one action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.`;
        choice = spoken;
        if (!choice) return `None of the selected cards has an action called "${displayText(wanted.label, DISPLAY_LIMITS.name)}", so nothing was sent.`;
      }
      if (!choice) return "None of the selected cards offers that action any more, so nothing was sent.";
      return startBatch(cards, choice.id, choice.label, `${cards.length} selected ${cards.length === 1 ? "agent" : "agents"}`);
    },
    sendCardBatch: () => {
      const current = latest.current.batch;
      if (!current || current.stage !== "review" || !invokeBatch || batchSending.current) return;
      const run = ++batchRun.current;
      const confirmed = current.groups
        .map((group) => ({ group, included: group.entries.filter((entry) => entry.outcome === "included") }))
        .filter(({ included }) => included.length > 0);
      if (confirmed.length === 0) return;
      batchSending.current = true;
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
          /* The bridge answers every entry for each failure it can place,
             before the request left (`failed`) or after (`unknown`). What
             reaches here it could not place, so no entry says it was not
             sent: whether it was is not known. */
          const message = `${cause instanceof Error ? cause.message : String(cause)}; ${BATCH_NOT_KNOWN}`;
          return { deckId: group.deckId, instanceId: group.instanceId, entries: included.map((entry) => ({ pane_id: entry.pane_id, agent_id: entry.agent_id, outcome: "unknown", message })) };
        }
      })).then((results) => {
        batchSending.current = false;
        if (batchRun.current === run) setBatch((shown) => shown && { ...shown, stage: "done", results, refusal: undefined });
      });
    },
    closeCardBatch: () => {
      /* Refused while it is being sent, by every route that comes here
         (Cancel is disabled; Escape and voice's `close` get this sentence):
         the daemons go on acting, and closing would lose the result for each
         agent. */
      if (batchSending.current) {
        setBatch((current) => current && { ...current, refusal: BATCH_STILL_SENDING });
        return BATCH_STILL_SENDING;
      }
      batchRun.current += 1;
      setBatch(undefined);
      return undefined;
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
      /* The page opens over the dashboard and any pane: a review left open
         would be drawn over it and fence it, so it closes (issue #2418
         review, F1). */
      setReview(undefined);
      setNotice(undefined);
      return undefined;
    },
  }), [changeOpenCards, find, invokeBatch, openAgent, remember, send, startBatch]);

  const reviewView = useMemo<CardReviewView | undefined>(() => {
    if (!review || !shown) return undefined;
    const card = placed.find((candidate) => placedCardKey(candidate) === review.key);
    return { card, lastName: review.name, actionId: review.actionId, token: review.token, sent: review.sent === true, results: results[review.key] ?? {}, ...(review.reviewed ? { reviewed: review.reviewed } : {}) };
  }, [placed, results, review, shown]);

  const reviewOpen = reviewView !== undefined;
  const voice = useMemo<Partial<AgentCardControls>>(() => (
    reviewOpen ? controls : { ...controls, closeCardReview: undefined }
  ), [controls, reviewOpen]);

  const clickControls = useMemo(() => ({ ...controls, reportRefused: setNotice }), [controls]);
  const dismissNotice = useCallback(() => setNotice(undefined), []);

  const resultsFor = useCallback((card: PlacedCard) => results[placedCardKey(card)] ?? {}, [results]);
  /* Issue #2399 — the selection is offered while any deck on screen does batches. */
  const anyBatches = shown && placed.some((card) => batchFor(card.deckId));
  const selection = useMemo<CardSelection | undefined>(() => {
    if (!anyBatches) return undefined;
    /* Read through the same rule as a batch, so the render before the pruning above lands never counts a card no action reaches. */
    const cards = selectedBatchCards(placed, selected, batchFor);
    const reachable = cards.length === selected.size ? selected : new Set(cards.map(placedCardKey));
    return { selected: reachable, last: lastToggled, choices: batchChoices(cards), deckIds: [...new Set(cards.map((card) => card.deckId))] };
  }, [anyBatches, batchFor, placed, selected, lastToggled]);
  return { shown, mode, needsYouOnly, placed, decksWithCards, openCards, treeFor, review: reviewView, notice, dismissNotice, rowMenu, controls, clickControls, voice, cardFor, resultsFor, batchFor, selection, batch: shown ? batch : undefined };
}
