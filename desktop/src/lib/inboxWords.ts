import type { AgentInbox } from "../types";

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
} as const;

/**
 * The card's inbox line (PRD #1666 §2.8): `✉2 queued` while messages are queued
 * or taken but not acknowledged, `✉ delivered` once the agent has acknowledged
 * one and nothing is pending, and nothing for an inbox that has held neither.
 * The TUI's card prints the same line.
 */
export function inboxBadge(inbox: AgentInbox | undefined): string | undefined {
  if (!inbox) return undefined;
  const pending = inbox.queued + inbox.fetched;
  if (pending > 0) return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.queued}`;
  return inbox.lastAckedAt ? `${INBOX_WORDS.glyph} ${INBOX_WORDS.delivered}` : undefined;
}
