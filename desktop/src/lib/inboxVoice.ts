import type { ConfirmState } from "../components/ConfirmDialog";
import type { AgentSession, AgentTarget, InboxActionResult, InboxActionVerb, InboxAttentionItem } from "../types";
import { AGENT_GONE_AFTER_RESTART } from "./agentIdentity";
import { sameIncarnation } from "./agentKey";
import { agentTitle } from "./agentNames";
import { DISPLAY_LIMITS, displayIdentity, displayText } from "./displayText";
import { inboxActionText } from "./inboxWords";

/**
 * Issue #2148 — Retry and Cancel on an agent's deck messages, by voice.
 *
 * A deck message has no name a user would say (its subject and sender are
 * whatever another agent wrote), so a spoken Retry or Cancel names the agent
 * the message was sent TO, and covers every message that agent's tile lists
 * for the verb: all of them for Cancel, all but those out of attempts for
 * Retry, which the tile does not offer on them either. Like the spoken stops
 * it only opens a confirmation, which lists each message by sender and subject;
 * the confirmation's button sends one Retry or Cancel per message, bound to the
 * revision shown, so a message another client acted on meanwhile is answered
 * as stale by the daemon rather than acted on again.
 */

/** A confirmation is already open; a second would replace it under the user's pointer. */
export const INBOX_CONFIRMATION_ALREADY_OPEN = "A confirmation is already open — answer it first. No message was retried or cancelled.";
/** The agent a spoken Retry or Cancel named is not on the screen any more. */
export const INBOX_TARGET_GONE = "That agent is not on the Daemons screen any more, so no message was retried or cancelled.";
export const INBOX_TARGET_RESTARTED = `${AGENT_GONE_AFTER_RESTART[0].toUpperCase()}${AGENT_GONE_AFTER_RESTART.slice(1)}, so no message was retried or cancelled.`;
/** Under All daemons the screen draws no tiles, so no message list is on screen to act on. */
export const INBOX_NEEDS_ONE_DAEMON = "Retry and Cancel on a deck message work with one daemon selected, where its agents' tiles list the messages — choose a daemon first.";

/** Nothing on that agent's tile waits on Retry or Cancel. */
export function inboxNothingWaiting(name: string): string {
  return `No deck message to ${name} is waiting on Retry or Cancel.`;
}

/** Every message on that agent's tile is out of attempts, which offers Cancel alone. */
export function inboxNothingToRetry(name: string, count: number): string {
  return count === 1
    ? `The deck message to ${name} is out of attempts, so it can only be cancelled: say "cancel the message to ${name}".`
    : `The deck messages to ${name} are out of attempts, so they can only be cancelled: say "cancel the messages to ${name}".`;
}

/** The messages a verb covers on one agent's tile: Retry is not offered on a message out of attempts. */
function inboxItemsFor(verb: InboxActionVerb, items: readonly InboxAttentionItem[]): InboxAttentionItem[] {
  return verb === "retry" ? items.filter((item) => item.kind !== "out_of_attempts") : [...items];
}

/** One message, as the confirmation lists it: its sender and its subject, both display text from other agents. */
function messageLine(item: InboxAttentionItem): string {
  const from = displayIdentity(item.fromLabel, DISPLAY_LIMITS.name, "an unnamed sender");
  const subject = displayText(item.subject, DISPLAY_LIMITS.name);
  return subject.trim() === "" ? `from ${from}` : `from ${from}: “${subject}”`;
}

type InboxAction = (target: AgentTarget, item: { paneId: string; messageId: string; revision: number }, verb: InboxActionVerb) => Promise<InboxActionResult>;

/**
 * Why a confirmation already pressed should send nothing more, read before
 * each message it covers: the user closed it, or the agent it was asked
 * about is no longer on the Daemons screen (another daemon was selected,
 * or its daemon restarted). `undefined` while it should go on.
 */
export type InboxStillWanted = (question: ConfirmState) => string | undefined;

/** The confirmation was closed while its messages were being sent (PR #2458 review). */
export const INBOX_STOPPED_CLOSED = "the confirmation was closed";
/** The agent left the screen between the question and the press, or during the run. */
export const INBOX_STOPPED_GONE = "that agent is no longer on the Daemons screen";

/** One result per distinct outcome, with its count when several messages share it, so the toast stays short however many messages the run covered. */
function summary(outcomes: readonly string[], many: boolean): string {
  const counts = new Map<string, number>();
  for (const outcome of outcomes) counts.set(outcome, (counts.get(outcome) ?? 0) + 1);
  return [...counts].map(([text, count]) => (many ? `${text} (${count})` : text)).join("; ");
}

/**
 * The confirmation a spoken Retry or Cancel opens for `agent`, or the sentence
 * saying why none opens. `onAnswer` hears one sentence once the run ends: each
 * distinct outcome in the row's own words, counted, and, when `stillWanted`
 * stopped it early, how far it got and why.
 */
export function inboxActionConfirmation(
  verb: InboxActionVerb,
  agent: Pick<AgentSession, "id" | "daemonId" | "displayName" | "tab" | "paneId" | "incarnation" | "inbox">,
  inboxAction: InboxAction,
  onAnswer: (sentence: string) => void,
  stillWanted: InboxStillWanted = () => undefined,
): ConfirmState | string {
  const name = agentTitle(agent).shown;
  const waiting = agent.inbox?.attention ?? [];
  const paneId = agent.paneId;
  if (waiting.length === 0 || !paneId) return inboxNothingWaiting(name);
  const items = inboxItemsFor(verb, waiting);
  if (items.length === 0) return inboxNothingToRetry(name, waiting.length);
  const many = items.length > 1;
  const noun = many ? `${items.length} deck messages` : "the deck message";
  const list = items.map(messageLine).join("; ");
  const skipped = waiting.length - items.length;
  const skippedNote = skipped === 0 ? "" : skipped === 1
    ? " One more message is out of attempts, so it is left for Cancel."
    : ` ${skipped} more messages are out of attempts, so they are left for Cancel.`;
  const target: AgentTarget = { deckId: agent.daemonId, agentId: agent.id, incarnation: agent.incarnation };
  const Verb = verb === "retry" ? "Retry" : "Cancel";
  const action = async () => {
    const outcomes: string[] = [];
    let stopped: string | undefined;
    for (const item of items) {
      stopped = stillWanted(question);
      if (stopped !== undefined) break;
      let text: string;
      try {
        const result = await inboxAction(target, { paneId, messageId: item.messageId, revision: item.revision }, verb);
        text = inboxActionText(verb, result.outcome);
      } catch (cause) {
        text = `failed: ${displayText(cause instanceof Error ? cause.message : String(cause), DISPLAY_LIMITS.name)}`;
      }
      outcomes.push(text);
    }
    if (stopped === undefined) {
      onAnswer(`${Verb} for ${name}: ${summary(outcomes, many)}.`);
    } else if (outcomes.length === 0) {
      onAnswer(`${Verb} for ${name} sent nothing: ${stopped}.`);
    } else {
      onAnswer(`${Verb} for ${name} stopped after ${outcomes.length} of ${items.length} messages, because ${stopped}: ${summary(outcomes, true)}.`);
    }
  };
  const question: ConfirmState = verb === "retry"
    ? {
      title: `Retry ${noun} to ${name}?`,
      body: `The deck sends ${many ? "each one" : "it"} again, marked as a possible repeat, so ${name} may get ${many ? "a message" : "it"} twice: ${list}.${skippedNote}`,
      label: "Retry",
      busyLabel: "Retrying…",
      cancelLabel: "No",
      neutral: true,
      action,
    }
    : {
      title: `Cancel ${noun} to ${name}?`,
      body: `The deck stops delivering ${many ? "them" : "it"}, and cannot take back a copy ${name} already has: ${list}.`,
      label: many ? "Cancel messages" : "Cancel message",
      busyLabel: "Cancelling…",
      cancelLabel: many ? "Keep them" : "Keep it",
      action,
    };
  return question;
}

/** What the deck screen shows now, read again before each message a pressed confirmation sends. */
export type InboxScreenNow = { agents: readonly AgentSession[]; allDecks: boolean; confirm: ConfirmState | undefined };

/**
 * The deck screen's `confirmInboxAction`, minus the screen: finds the agent a
 * voice target names among the agents the screen shows, and answers the
 * confirmation to open or the sentence saying why not. `now` is read before
 * each message once the confirmation is pressed, so a confirmation the user
 * closed, or one left open while another daemon was selected or the agent's
 * daemon restarted, sends nothing more (PR #2458 review).
 */
export function confirmInboxActionFor(
  verb: InboxActionVerb,
  target: { deckId?: string; agentId: string; incarnation?: string },
  screen: { agents: readonly AgentSession[]; allDecks: boolean; confirmationOpen: boolean; inboxAction?: InboxAction },
  onAnswer: (sentence: string) => void,
  now?: () => InboxScreenNow,
): ConfirmState | string {
  if (screen.confirmationOpen) return INBOX_CONFIRMATION_ALREADY_OPEN;
  if (screen.allDecks) return INBOX_NEEDS_ONE_DAEMON;
  const agent = screen.agents.find((candidate) => candidate.id === target.agentId && (target.deckId === undefined || candidate.daemonId === target.deckId));
  if (!agent || !screen.inboxAction) return INBOX_TARGET_GONE;
  if ("incarnation" in target && !sameIncarnation(target.incarnation, agent.incarnation)) return INBOX_TARGET_RESTARTED;
  const stillWanted: InboxStillWanted = (question) => {
    if (!now) return undefined;
    const shown = now();
    if (shown.confirm !== question) return INBOX_STOPPED_CLOSED;
    const here = !shown.allDecks && shown.agents.some((candidate) => candidate.daemonId === agent.daemonId && candidate.id === agent.id && sameIncarnation(candidate.incarnation, agent.incarnation));
    return here ? undefined : INBOX_STOPPED_GONE;
  };
  return inboxActionConfirmation(verb, agent, screen.inboxAction, onAnswer, stillWanted);
}
