import type { AgentInbox, InboxActionVerb, InboxAttentionItem } from "../types";

/**
 * The words both clients use for a message's journey through an agent's inbox
 * (PRD #1666, CLAUDE.md rule 22). The TUI prints the same strings from
 * `src/inbox_display.rs`, and `tests/fixtures/inbox-display.json` pins the two
 * to each other: change a word here and there together.
 */
export const INBOX_WORDS = {
  /** Prefixes the card's pending count (`✉2`) and a deck message's prompt line. */
  glyph: "✉",
  /** Messages waiting for the agent, taken or not. */
  queued: "queued",
  /** The agent's side acknowledged the message: its content reached the agent. */
  delivered: "delivered",
  /** The message could not reach the agent. */
  notDelivered: "not delivered",
  /** The worker reported back on a delegated task. */
  workDone: "work-done",
  /** Messages have waited longer than a wake-up takes while the agent was not working. */
  wakeStalled: "waiting to wake",
  /** What the user can do about `wakeStalled`. */
  wakeStalledHint: "send it a prompt",
  uncertain: "delivery uncertain",
  uncertainMany: "deliveries uncertain",
  uncertainReason: "a deck message may or may not have reached this agent. Retry sends it again marked as a possible repeat; Cancel stops delivering it.",
  outOfAttempts: "out of attempts: the deck sent this message as many times as it allows. Cancel it, or post it again.",
  heldLong: "the agent has had a deck message for an hour without confirming it. It may still be working on it.",
  retry: "Retry",
  cancel: "Cancel",
  retryLimit: "retry limit reached; Cancel it, or post it again",
  actionStale: "another client already acted on this message; the card shows where it stands now",
  actionSettled: "it reached the agent after all",
  actionGone: "this message no longer needs a decision",
  retryApplied: "sent again, marked as a possible repeat",
  cancelApplied: "delivery cancelled",
  actionUnsupported: "this deck cannot Retry or Cancel deck messages: upgrade dot-agent-deck",
  attentionTitle: "Deck messages",
  adapterTooOld: "this agent's deck integration is out of date; restart the agent to update it",
  cliTooOld: "the dot-agent-deck command this agent runs does not support this deck's integration: upgrade dot-agent-deck, then restart the agent",
  notArmed: "this agent's deck integration has not connected yet; its messages wait until it does",
  refreshFailed: "the deck could not update this agent's deck integration: run dot-agent-deck hooks install --agent opencode to see why, then restart the agent",
  receiptsDegraded: "messages may repeat: the deck cannot record message receipts for this agent",
  evidencePaused: "deck messages paused: {n} delivered messages still wait for proof that older copies cannot run; restart the agent to clear them",
} as const;

const uncertainCount = (inbox: AgentInbox) =>
  (inbox.attention ?? []).filter((item) => item.kind !== "held_long").length;

/**
 * The card's inbox line (PRD #1666 §2.8): `✉2 queued` while messages are queued
 * or taken but not acknowledged, `✉2 waiting to wake: send it a prompt` while
 * the deck reports nothing has woken the agent for them, `✉ delivered` once the
 * agent has acknowledged one and nothing is pending, and nothing for an inbox
 * that has held neither. The TUI's card prints the same line.
 */
export function inboxBadge(inbox: AgentInbox | undefined): string | undefined {
  if (!inbox) return undefined;
  const uncertain = uncertainCount(inbox);
  if (uncertain === 1) return `${INBOX_WORDS.glyph} ${INBOX_WORDS.uncertain}`;
  if (uncertain > 1) return `${INBOX_WORDS.glyph} ${uncertain} ${INBOX_WORDS.uncertainMany}`;
  const pending = inbox.queued + inbox.fetched;
  if (pending > 0 && inbox.wakeStalled) {
    return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.wakeStalled}: ${INBOX_WORDS.wakeStalledHint}`;
  }
  if (pending > 0) return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.queued}`;
  return inbox.lastAckedAt ? `${INBOX_WORDS.glyph} ${INBOX_WORDS.delivered}` : undefined;
}

/**
 * PRD #1666 §6.5: the card's inbox notice row, when the deck has something to
 * say beyond the badge — the first that holds of: why the integration takes
 * nothing yet, paused hand-outs, what an uncertain delivery means, held_long
 * attention, failed receipt storage. The TUI's card prints the same row.
 */
export function inboxNotice(inbox: AgentInbox | undefined): string | undefined {
  if (!inbox) return undefined;
  if (inbox.integration === "cli_too_old") return INBOX_WORDS.cliTooOld;
  if (inbox.integration === "adapter_too_old") return INBOX_WORDS.adapterTooOld;
  if (inbox.integration === "not_armed") return INBOX_WORDS.notArmed;
  if (inbox.integration === "refresh_failed") return INBOX_WORDS.refreshFailed;
  if (inbox.evidencePaused !== undefined) return INBOX_WORDS.evidencePaused.replace("{n}", String(inbox.evidencePaused));
  if (uncertainCount(inbox) > 0) return INBOX_WORDS.uncertainReason;
  if ((inbox.attention ?? []).length > 0) return INBOX_WORDS.heldLong;
  if (inbox.receiptsDegraded !== undefined) return `${INBOX_WORDS.receiptsDegraded} (${inbox.receiptsDegraded})`;
  return undefined;
}

/** PRD #1666 §6.4: what a listed item's row explains. */
export function attentionReason(kind: InboxAttentionItem["kind"]): string {
  if (kind === "out_of_attempts") return INBOX_WORDS.outOfAttempts;
  if (kind === "held_long") return INBOX_WORDS.heldLong;
  return INBOX_WORDS.uncertainReason;
}

/** PRD #1666 §2: the row's words for the daemon's answer to Retry or Cancel. */
export function inboxActionText(verb: InboxActionVerb, outcome: string): string {
  switch (outcome) {
    case "applied": return verb === "retry" ? INBOX_WORDS.retryApplied : INBOX_WORDS.cancelApplied;
    case "stale": return INBOX_WORDS.actionStale;
    case "already_settled": return INBOX_WORDS.actionSettled;
    case "retry_limit": return INBOX_WORDS.retryLimit;
    case "unsupported": return INBOX_WORDS.actionUnsupported;
    default: return INBOX_WORDS.actionGone;
  }
}
