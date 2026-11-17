import type { AgentCardAction, AgentCardEntry, AgentCardsSnapshot, AgentCardTarget, CardActionResult } from "../types";
import { sanitizeText } from "./displayText";

/*
 * PRD #2374 — the overview's agent cards, as the webview arranges them.
 *
 * Everything here ARRANGES what the daemon sent; nothing DECIDES. Status,
 * needs-you, which actions exist, whether each is enabled and the order the
 * entries come in are the daemon's (CLAUDE.md rule 18): an entry is rendered
 * as received, consecutive entries sharing a `group.id` are one group, and the
 * counts are counts of what arrived. The one judgement made here is the
 * user's own — whether the Needs you filter is on — and it only hides entries
 * the daemon already marked.
 */

/** Which view the overview shows. */
export type OverviewMode = "table" | "cards";

/** Where the overview remembers the mode, per client (this window's storage). */
export const OVERVIEW_MODE_STORAGE_KEY = "dot-agent-deck.overview.mode";

/** The remembered mode; `table` when nothing usable is stored or storage is unavailable. */
export function readOverviewMode(): OverviewMode {
  try {
    return window.localStorage.getItem(OVERVIEW_MODE_STORAGE_KEY) === "cards" ? "cards" : "table";
  } catch {
    return "table";
  }
}

/** Remember `mode`; a storage that refuses the write leaves the choice for this session only. */
export function writeOverviewMode(mode: OverviewMode): void {
  try {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, mode);
  } catch {
    // Unavailable or full storage: the mode still applies until the window closes.
  }
}

/** One card on screen: the entry, and the deck it is on. */
export interface PlacedCard {
  deckId: string;
  entry: AgentCardEntry;
}

/**
 * A card's identity across the fleet: the deck, the daemon process that listed
 * it, and its key. Two daemons number their agents independently and a
 * restarted daemon numbers them again, so the key alone names nothing.
 */
export function placedCardKey(card: Pick<PlacedCard, "deckId"> & { entry: Pick<AgentCardEntry, "instance_id" | "key"> }): string {
  return `${card.deckId}\u0000${card.entry.instance_id}\u0000${card.entry.key}`;
}

/** What a card action is sent to: this entry, as the daemon process that listed it. */
export function cardTarget(card: PlacedCard): AgentCardTarget {
  return { deckId: card.deckId, instanceId: card.entry.instance_id, paneId: card.entry.pane_id, agentId: card.entry.agent_id };
}

/**
 * Every card of the decks given, in their order and each deck's entries in the
 * daemon's display order. A deck that is not answering, or whose daemon has no
 * cards, contributes none.
 */
export function placeCards(decks: readonly { deckId: string | undefined; connected: boolean }[], cards: Readonly<Record<string, AgentCardsSnapshot>> | undefined): PlacedCard[] {
  if (!cards) return [];
  return decks.flatMap((deck) => {
    if (!deck.connected || deck.deckId === undefined) return [];
    const snapshot = cards[deck.deckId];
    const deckId = deck.deckId;
    return snapshot ? snapshot.entries.map((entry) => ({ deckId, entry })) : [];
  });
}

/** One group as the grid shows it: consecutive cards sharing a `group.id`, and their counts. */
export interface CardGroup {
  /** Unique among the groups `groupCards` returns: the run's place, the deck and the `group.id`. */
  key: string;
  label: string;
  kind: string;
  cards: PlacedCard[];
  /** How many of every card in this group (shown or not) need the user. */
  needYou: number;
  /** How many cards the group has, shown or not. */
  total: number;
}

/** The heading a group gets when the daemon gave it no label. */
function groupLabel(entry: AgentCardEntry): string {
  if (entry.group.label) return entry.group.label;
  if (entry.group.kind === "standalone") return "Not in a group";
  return entry.group.kind;
}

/**
 * The grid's groups: consecutive cards of one deck that share a `group.id`.
 * `needsYouOnly` hides the cards the daemon did not mark, and a group left
 * with none is dropped; the counts are always over the whole group.
 */
export function groupCards(cards: readonly PlacedCard[], needsYouOnly: boolean): CardGroup[] {
  const groups: CardGroup[] = [];
  let lastId: string | undefined;
  for (const card of cards) {
    // The deck is part of the id: two daemons' groups never merge.
    const id = `${card.deckId}\u0000${card.entry.group.id}`;
    const last = groups[groups.length - 1];
    // The same group may come round again later in the daemon's order (a run
    // of its members, another group, then more of them): that is a second
    // group on screen, so the run's place is part of its key, which React
    // needs unique among the grid's groups.
    const group: CardGroup = last !== undefined && lastId === id ? last : { key: `${groups.length}\u0000${id}`, label: groupLabel(card.entry), kind: card.entry.group.kind, cards: [], needYou: 0, total: 0 };
    lastId = id;
    if (group !== last) groups.push(group);
    group.total += 1;
    if (card.entry.needs_you) group.needYou += 1;
    if (!needsYouOnly || card.entry.needs_you) group.cards.push(card);
  }
  return needsYouOnly ? groups.filter((group) => group.cards.length > 0) : groups;
}

/** Where the overlay moves: one card on, one back, or to the next card that needs the user. */
export type CardStep = "next" | "previous" | "needs-you";

/**
 * The card a step lands on, from the card at `from` in `order` (the display
 * order, wrapping); `undefined` when there is nowhere to go — an empty order,
 * or no other card that needs the user. `from` may be `-1` when the open card
 * is no longer listed: a step then starts from the beginning.
 */
export function stepCard(order: readonly PlacedCard[], from: number, step: CardStep): number | undefined {
  const count = order.length;
  if (count === 0) return undefined;
  if (step === "next") return from < 0 ? 0 : (from + 1) % count;
  if (step === "previous") return from < 0 ? count - 1 : (from - 1 + count) % count;
  for (let offset = 1; offset <= count; offset += 1) {
    const at = (Math.max(from, -1) + offset + count) % count;
    if (at !== from && order[at].entry.needs_you) return at;
  }
  return undefined;
}

/** The action a card's primary button presses, as the daemon chose it. */
export function primaryAction(entry: AgentCardEntry): AgentCardAction | undefined {
  return entry.actions.find((action) => action.id === entry.primary_action_id);
}

/** The words for a deck-checked status (the daemon's snake_case). */
export function cardStatusLabel(status: string): string {
  const words = status.replace(/_/g, " ").trim();
  return words ? `${words[0].toUpperCase()}${words.slice(1)}` : "Unknown";
}

/** The words for the agent's own reported status. */
export function reportStatusLabel(status: string): string {
  return cardStatusLabel(status);
}

/** Why a card needs the user, in words, for its marker's hover and the overlay. */
const NEEDS_YOU_REASON: Record<string, string> = {
  stopped: "its agent stopped",
  native_question: "the agent is asking a question in its terminal",
  question: "the agent asked a question on its card",
  waiting: "the agent says it is waiting for you",
  done: "the agent says it is done",
  failed: "the agent says it failed",
  blocked: "the agent is blocked",
};

/** The reasons a card needs the user, as one sentence; empty when it does not. */
export function needsYouSentence(entry: Pick<AgentCardEntry, "needs_you" | "needs_you_reasons">): string {
  if (!entry.needs_you) return "";
  const reasons = entry.needs_you_reasons.map((reason) => NEEDS_YOU_REASON[reason] ?? reason.replace(/_/g, " "));
  return reasons.length ? `Needs you: ${reasons.join("; ")}.` : "Needs you.";
}

/** Why an action is disabled, in words: the daemon's reason code, read as a sentence. */
const DISABLED_REASON: Record<string, string> = {
  agent_stopped: "the agent is not running",
};

export function disabledReasonSentence(reason: string | null): string {
  if (!reason) return "";
  return DISABLED_REASON[reason] ?? reason.replace(/_/g, " ");
}

/** What a delivery state says to the user. */
const DELIVERY_STATE: Record<string, string> = {
  queued: "Queued for the agent",
  delivered: "Delivered to the agent",
  uncertain: "Delivery uncertain",
  not_delivered: "Not delivered",
};

export function deliverySentence(delivery: NonNullable<AgentCardAction["last_delivery"]>): string {
  const state = DELIVERY_STATE[delivery.state] ?? delivery.state;
  return delivery.detail ? `${state} (${delivery.detail.replace(/_/g, " ")})` : state;
}

/** A field's value as text: lists joined, booleans in words, everything else as it came. */
export function fieldValueText(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (Array.isArray(value)) return value.map((item) => fieldValueText(item)).join(", ");
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (typeof value === "string") return value;
  if (typeof value === "number") return String(value);
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/** A field value the overlay may offer as a link: an `http(s)` URL and nothing else. */
export function linkOf(type: string, value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  if (type !== "url" && type !== "pull_request" && type !== "issue") return undefined;
  return /^https?:\/\//i.test(value) ? value : undefined;
}

/**
 * The action a spoken label names on `entry`: an action's label, or a question
 * option's label (its action is `question/<q>/<o>`). Matched ignoring case and
 * surrounding punctuation; `undefined` when none or more than one matches.
 */
export function actionBySpokenLabel(entry: AgentCardEntry, spoken: string): AgentCardAction | undefined {
  const norm = (value: string) => value.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  const wanted = norm(spoken);
  if (!wanted) return undefined;
  const optionLabels = new Map<string, string>();
  for (const question of entry.questions) for (const option of question.options) optionLabels.set(option.action_id, option.label);
  const labelOf = (action: AgentCardAction) => optionLabels.get(action.id) ?? action.label;
  const exact = entry.actions.filter((action) => norm(labelOf(action)) === wanted || norm(action.id) === wanted);
  if (exact.length === 1) return exact[0];
  if (exact.length > 1) return undefined;
  const partial = entry.actions.filter((action) => norm(labelOf(action)).includes(wanted));
  return partial.length === 1 ? partial[0] : undefined;
}

/** The label an action shows: a question option's own label, else the action's. */
export function actionLabel(entry: AgentCardEntry, action: AgentCardAction): string {
  for (const question of entry.questions) {
    const option = question.options.find((candidate) => candidate.action_id === action.id);
    if (option) return option.label;
  }
  return action.label;
}

/** The first 60 characters of a prompt, for the card's preview under its primary button. */
export function promptPreview(prompt: string, max = 60): string {
  const flat = prompt.replace(/\s+/g, " ").trim();
  return flat.length <= max ? flat : `${flat.slice(0, max - 1)}…`;
}

/**
 * Agent-written text that may span lines (a question, a `long_text` field, a
 * prompt), made safe to show: each line sanitised like any daemon string, the
 * line breaks kept. Rendered as text with `white-space: pre-wrap`, never as markup.
 */
export function multilineText(value: string): string {
  return value.split("\n").map((line) => sanitizeText(line)).join("\n");
}

/** What the overlay is doing besides showing the card: asking to confirm one action, or taking a reply. */
export type CardOverlayStep = { kind: "confirm"; actionId: string } | { kind: "reply" };

/** The answer to the last press of each action on the open card, by action id, as the daemon gave it. */
export type CardResults = Readonly<Record<string, CardActionResult | { outcome: "pending" } | { outcome: "failed"; message: string }>>;

/** The sentence for an invoke's answer. */
export function resultSentence(result: CardResults[string]): string {
  switch (result.outcome) {
    case "pending":
      return "Sending…";
    case "failed":
      return `Not sent: ${result.message}`;
    case "sent":
      return "message_id" in result && result.message_id ? `Sent (message ${result.message_id}).` : "Sent.";
    case "closed":
      return "kept_worktree" in result && result.kept_worktree ? `Closed; kept its worktree at ${result.kept_worktree}.` : "Closed.";
    case "refused":
      return `Refused: ${"message" in result && result.message ? result.message : "code" in result && result.code ? result.code : "the daemon did not say why"}`;
    default:
      return String(result.outcome);
  }
}

