import type { AgentCardAction, AgentCardEntry, AgentCardsSnapshot, AgentCardTarget, CardActionResult } from "../types";
import { sanitizeText } from "./displayText";
import { cardStatusWord, statusWordLabel } from "./statusWords";

/*
 * PRD #2374 — the overview's agent cards, as the webview arranges them.
 *
 * Everything here ARRANGES what the daemon sent; nothing DECIDES. Status,
 * needs-you, which actions exist, whether each is enabled and the order the
 * entries come in are the daemon's (CLAUDE.md rule 18): an entry is rendered
 * as received, and so are the tree it hangs in and its roll-up (issue #2418,
 * `agentCardsTree.ts`). The judgements made here are the user's own — which
 * view, which trees are open, whether Needs you only is on.
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

/** What a card action is sent to: this entry, as the daemon process that listed it (the action's `token` goes with the press). */
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

/** Where the pane's header moves: one agent on, one back, or to the next one that needs the user. */
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

/**
 * The words for a deck-checked status: the daemon's own, as the table and the
 * TUI show them (`statusWords.ts`, issue #2411) — `waiting_for_input` reads
 * "needs input" and `unknown` reads "idle". A status this build does not know
 * is shown as it came.
 */
export function cardStatusLabel(status: string): string {
  const word = cardStatusWord(status);
  return word ? statusWordLabel(word) : status.replace(/_/g, " ").trim() || "unknown";
}

/** The CSS status class of a deck-checked status: its word's, so the card shares the table's palette. */
export function cardStatusClass(status: string): string {
  return `status-${cardStatusWord(status) ?? "unknown"}`;
}

/** The words for the agent's own reported status (`working`, `waiting`, `done`, `failed`): the agent's word, not the daemon's. */
export function reportStatusLabel(status: string): string {
  const words = status.replace(/_/g, " ").trim();
  return words ? `${words[0].toUpperCase()}${words.slice(1)}` : "Unknown";
}

/** Why a card needs the user, in words, for its marker's hover and the Card tab. */
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

/**
 * Why a delivery is uncertain or was not delivered, in words: the inbox's
 * attention flag or undeliverable reason, as the daemon names it. A detail
 * this build does not know is shown with its underscores as spaces.
 */
const DELIVERY_DETAIL: Record<string, string> = {
  out_of_attempts: "the deck sent it as many times as it allows; see the agent's inbox",
  not_started: "the agent has not started on it; see the agent's inbox",
  held_long: "the agent has had it for an hour without confirming it",
  agent_exited: "the agent exited",
  max_deliver: "out of attempts",
  superseded: "a newer message replaced it",
  inbox_shut: "the deck was shutting down",
  cancelled: "cancelled",
};

/** A delivery state in words, as a card's delivery history says it (`queued` reads "Queued for the agent"). */
export function deliveryStateWord(state: string): string {
  return DELIVERY_STATE[state] ?? state.replace(/_/g, " ");
}

export function deliverySentence(delivery: Pick<NonNullable<AgentCardAction["last_delivery"]>, "state" | "detail">): string {
  const state = DELIVERY_STATE[delivery.state] ?? delivery.state;
  if (!delivery.detail) return state;
  return `${state} (${DELIVERY_DETAIL[delivery.detail] ?? delivery.detail.replace(/_/g, " ")})`;
}

/** When a delivery happened, as a local clock time; `—` for a time that does not parse. */
export function formatClockTime(at: string): string {
  const parsed = new Date(at);
  if (Number.isNaN(parsed.getTime())) return "—";
  return parsed.toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" });
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

/**
 * What a spoken label names on a card (PR #2491 review J3, audit K2): one
 * action, none, or more than one — an ambiguous label is refused, never read
 * as "none" and retried as something else.
 */
export type SpokenLabelMatch =
  | { outcome: "found"; action: AgentCardAction }
  | { outcome: "none" }
  | { outcome: "ambiguous"; count: number };

/**
 * The action a spoken label names on `entry`: an action's label, or a question
 * option's label (its action is `question/<q>/<o>`). Matched ignoring case and
 * surrounding punctuation. The command model gives back "answer main" for
 * "answer main to the coder" — the verb the user said, then the button's
 * words — so the phrase is tried again without a leading `answer`, `press`,
 * `send` or `say`, but only when it named nothing as said: a label that
 * starts with the verb itself is found whole, and one several buttons share
 * stays ambiguous.
 */
export function spokenLabelMatch(entry: AgentCardEntry, spoken: string): SpokenLabelMatch {
  const norm = (value: string) => value.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  const wanted = norm(spoken);
  if (!wanted) return { outcome: "none" };
  const said = matchSpokenLabel(entry, wanted, norm);
  if (said.outcome !== "none") return said;
  const bare = wanted.replace(/^(answer|press|send|say)\s+/, "");
  return bare !== wanted && bare ? matchSpokenLabel(entry, bare, norm) : said;
}

/** {@link spokenLabelMatch}'s action when it found exactly one; `undefined` for none and for an ambiguous label. */
export function actionBySpokenLabel(entry: AgentCardEntry, spoken: string): AgentCardAction | undefined {
  const match = spokenLabelMatch(entry, spoken);
  return match.outcome === "found" ? match.action : undefined;
}

function matchSpokenLabel(entry: AgentCardEntry, wanted: string, norm: (value: string) => string): SpokenLabelMatch {
  const optionLabels = new Map<string, string>();
  for (const question of entry.questions) for (const option of question.options) optionLabels.set(option.action_id, option.label);
  const labelOf = (action: AgentCardAction) => optionLabels.get(action.id) ?? action.label;
  const pick = (found: AgentCardAction[]): SpokenLabelMatch | undefined => (
    found.length === 1 ? { outcome: "found", action: found[0] } : found.length > 1 ? { outcome: "ambiguous", count: found.length } : undefined
  );
  return pick(entry.actions.filter((action) => norm(labelOf(action)) === wanted || norm(action.id) === wanted))
    ?? pick(entry.actions.filter((action) => norm(labelOf(action)).includes(wanted)))
    ?? { outcome: "none" };
}

/** The label an action shows: a question option's own label, else the action's. */
export function actionLabel(entry: AgentCardEntry, action: AgentCardAction): string {
  for (const question of entry.questions) {
    const option = question.options.find((candidate) => candidate.action_id === action.id);
    if (option) return option.label;
  }
  return action.label;
}

/**
 * The start of a prompt, for the card's preview under its primary button:
 * its first `max` characters as shown by {@link exactTextString} (invisible
 * characters as markers, nothing collapsed), with each line break as the
 * marker `⟦↵⟧`, and `…` when there is more. A marker is kept whole or left
 * out, never cut.
 */
export function promptPreview(prompt: string, max = 60): string {
  const pieces: string[] = [];
  for (const segment of exactTextSegments(prompt)) {
    if (segment.escaped) pieces.push(segment.shown);
    else for (const char of segment.raw) pieces.push(char === "\n" ? `${MARKER_OPEN}\u21b5${MARKER_CLOSE}` : char);
  }
  const length = (parts: string[]) => parts.reduce((total, part) => total + Array.from(part).length, 0);
  if (length(pieces) <= max) return pieces.join("");
  const kept: string[] = [];
  let used = 0;
  for (const piece of pieces) {
    const size = Array.from(piece).length;
    if (used + size > max - 1) break;
    kept.push(piece);
    used += size;
  }
  return `${kept.join("")}…`;
}

/**
 * Who may have written a prompt that is sent without the user reading it
 * first: only the catalog, the deck's defaults or the user's own (PRD #2374).
 * The daemon says which (`prompt_source`); anything else — the agent's text,
 * the answer the deck composes by quoting the agent's question (audit A1),
 * an absent value — is shown for review first.
 */
const SENDABLE_UNREAD: ReadonlySet<string> = new Set(["catalog"]);

/** Whether `action`'s prompt may be sent by a control that did not show all of it. */
export function sendableUnread(action: Pick<AgentCardAction, "prompt_source">): boolean {
  return SENDABLE_UNREAD.has(action.prompt_source ?? "");
}

/**
 * Whether the compact card's primary button must open the review step rather
 * than send (PRD #2374 review; issue #2418 audit A4): its prompt is not the
 * catalog's. The compact card draws the prompt on one line that the layout
 * cuts off at whatever width the card has, so a preview short enough to be
 * whole in characters can still be cut on screen; nothing on that line proves
 * the user saw the text. A catalog prompt is sent by the button, as before;
 * every other prompt (the agent's, the deck's answer quoting the agent's
 * question, one with no source) goes to the review step, which shows it whole.
 */
export function primaryNeedsReview(action: Pick<AgentCardAction, "prompt" | "prompt_source">): boolean {
  if (action.prompt === null || action.prompt === undefined) return false;
  return !sendableUnread(action);
}

/**
 * Agent-written text that may span lines (a question, a `long_text` field, a
 * prompt), made safe to show: each line sanitised like any daemon string, the
 * line breaks kept. Rendered as text with `white-space: pre-wrap`, never as markup.
 */
export function multilineText(value: string): string {
  return value.split("\n").map((line) => sanitizeText(line)).join("\n");
}

/** One piece of a prompt as the user is shown it: the exact characters, and how they are shown. */
export interface ExactTextSegment {
  /** The characters sent, exactly. */
  raw: string;
  /** What is shown for them: the same text, or a marker such as `⟦tab⟧` or `⟦U+202E⟧`. */
  shown: string;
  /** Whether `shown` is a marker standing for one character that is not shown as itself. */
  escaped: boolean;
}

/** What opens a marker. Every one in the shown text opens a marker: a literal one in the prompt is itself shown as a marker. */
export const MARKER_OPEN = "\u27E6";
/** What closes a marker. */
export const MARKER_CLOSE = "\u27E7";

/*
 * The characters a prompt preview shows as markers rather than as
 * themselves: control characters other than the line break (a tab included),
 * format characters (bidi overrides and isolates, zero-width characters, the
 * byte-order mark, the soft hyphen), the line and paragraph separators, and
 * every other default-ignorable code point — variation selectors, the
 * combining grapheme joiner, Hangul fillers and the like (audit A3). Shown as
 * themselves they are invisible, or reorder or split the text around them,
 * so the preview would not show what is sent (audit F3). The marker opener
 * itself is marked too, so a marker can never be typed as text (review R3).
 */
const MARKED = /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}\u27E6]/u;

/** The name inside one character's marker: `tab`, `CR`, or its code point. */
function markerName(char: string): string {
  if (char === "\t") return "tab";
  if (char === "\r") return "CR";
  return `U+${(char.codePointAt(0) ?? 0).toString(16).toUpperCase().padStart(4, "0")}`;
}

/** A character's marker: `⟦tab⟧`, `⟦U+202E⟧`. */
function markerOf(char: string): string {
  return `${MARKER_OPEN}${markerName(char)}${MARKER_CLOSE}`;
}

/** What the hover of a marker says it stands for. */
export function markerTitle(char: string): string {
  if (char === MARKER_OPEN) return `The character ${MARKER_OPEN} itself, sent as is`;
  return `A character you cannot see (${markerName(char)}), sent as is`;
}

/**
 * A prompt split into what it is and how to show it, so the preview renders
 * EXACTLY the text that is sent: nothing is dropped, merged or normalised,
 * line breaks stay line breaks, and every invisible or reordering character
 * is shown as a marker, `⟦…⟧` (audit F3, A3). The markers cannot be confused
 * with the prompt's own text: a literal `⟦` is itself shown as a marker, so
 * every `⟦` on screen opens one, and a typed `\t` or `\u{202E}` stays the
 * plain characters it is (review R3). Joining the `raw` parts gives the
 * prompt back character for character.
 */
export function exactTextSegments(value: string): ExactTextSegment[] {
  const segments: ExactTextSegment[] = [];
  let plain = "";
  for (const char of value) {
    if (char !== "\n" && MARKED.test(char)) {
      if (plain) segments.push({ raw: plain, shown: plain, escaped: false });
      plain = "";
      segments.push({ raw: char, shown: markerOf(char), escaped: true });
    } else {
      plain += char;
    }
  }
  if (plain) segments.push({ raw: plain, shown: plain, escaped: false });
  return segments;
}

/** Whether a prompt has any character shown as a marker. */
export function hasMarkers(value: string): boolean {
  return exactTextSegments(value).some((segment) => segment.escaped);
}

/** A prompt as one string with its markers, for a hover (`title`), which cannot style them; as unambiguous as the styled preview. */
export function exactTextString(value: string): string {
  return exactTextSegments(value).map((segment) => segment.shown).join("");
}


/**
 * The answer to the last press of each action definition on one card, by the
 * definition's `token`, as the daemon gave it. Keyed by token, so an action id
 * the agent reused for new text does not show the old text's answer.
 */
export type CardResults = Readonly<Record<string, CardActionResult | { outcome: "pending" } | { outcome: "failed"; message: string }>>;

/** A deck as the cards view needs it: whether it is connected, and the daemon process its connection names. */
export interface CardsConnection {
  connected: boolean;
  incarnation?: string;
}

/**
 * Whether `next` may replace `held` as a deck's cards (review B6): only for a
 * deck in the fleet that is connected, only from the daemon process that
 * connection names, and never an older revision of that process's cards than
 * the one held. Pushed snapshots and direct reads both pass through here.
 */
export function acceptCardsSnapshot(held: AgentCardsSnapshot | undefined, next: AgentCardsSnapshot, connection: CardsConnection | undefined): boolean {
  if (!connection?.connected) return false;
  if (connection.incarnation !== undefined && next.instanceId !== connection.incarnation) return false;
  return !(held && held.instanceId === next.instanceId && next.revision < held.revision);
}

/**
 * PR #2491 review L1 — what a deck's CURRENT cards listing says of one
 * agent's card, for a pane kept on that card while its agent is gone:
 *
 * - `listed`: the listing holds it;
 * - `omitted`: a listing from the daemon process the deck is connected to
 *   now leaves it out, or that process is not the one the card was for — the
 *   card is gone, and only this retires the pane;
 * - `awaiting`: no listing from the current process yet (a deck that has just
 *   reconnected answers its agents before its cards are read). The agents'
 *   list says nothing about the cards, so the pane waits.
 */
export type CardListing = "listed" | "omitted" | "awaiting";

export function cardListing(snapshot: AgentCardsSnapshot | undefined, connection: { incarnation?: string }, agentId: string, incarnation: string | undefined): CardListing {
  if (incarnation !== undefined && connection.incarnation !== undefined && connection.incarnation !== incarnation) return "omitted";
  if (!snapshot || (connection.incarnation !== undefined && snapshot.instanceId !== connection.incarnation)) return "awaiting";
  return snapshot.entries.some((entry) => entry.agent_id === agentId && (incarnation === undefined || entry.instance_id === "" || entry.instance_id === incarnation)) ? "listed" : "omitted";
}

/**
 * The cards still valid for `connections`: a deck that left the fleet or is
 * not connected keeps none, and neither does one whose connection now names
 * another daemon process than the one that listed its cards (a daemon
 * replaced while the deck stayed connected).
 */
export function reconcileCards(held: Readonly<Record<string, AgentCardsSnapshot>>, connections: ReadonlyMap<string, CardsConnection>): Readonly<Record<string, AgentCardsSnapshot>> {
  const kept = Object.entries(held).filter(([deckId, snapshot]) => {
    const connection = connections.get(deckId);
    return connection?.connected === true && (connection.incarnation === undefined || connection.incarnation === snapshot.instanceId);
  });
  return kept.length === Object.keys(held).length ? held : Object.fromEntries(kept);
}

/**
 * The results still worth showing: those of cards still listed, and of action
 * definitions those cards still offer — so the map is bounded by what is on
 * screen (audit F5). What the daemon delivered stays on the card itself
 * (`recent_deliveries`).
 */
export function pruneCardResults(results: Readonly<Record<string, CardResults>>, placed: readonly PlacedCard[]): Readonly<Record<string, CardResults>> {
  const tokens = new Map(placed.map((card) => [placedCardKey(card), new Set(card.entry.actions.map((action) => action.token))]));
  let changed = false;
  const next: Record<string, CardResults> = {};
  for (const [cardKey, byToken] of Object.entries(results)) {
    const offered = tokens.get(cardKey);
    if (!offered) {
      changed = true;
      continue;
    }
    const kept = Object.entries(byToken).filter(([token]) => offered.has(token));
    if (kept.length !== Object.keys(byToken).length) changed = true;
    if (kept.length) next[cardKey] = Object.fromEntries(kept);
  }
  return changed ? next : results;
}

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

