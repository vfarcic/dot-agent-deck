/**
 * Issue #2075: ONE operating-system notification per question an agent has
 * left unanswered for N minutes.
 *
 * Pure: the caller hands in the questions waiting now, the clock and N, and
 * gets back what to post. What has been posted is kept in a
 * {@link QuestionNotifyLedger} the caller owns, keyed by daemon, pane and
 * question id, so a question is never notified twice however often the fleet
 * is re-read, and a question answered before its N minutes is gone from the
 * waiting list before it could be notified at all. N of `0` posts nothing.
 *
 * What "never twice" rests on: a key, once posted, stays in the ledger until
 * the ledger is full, and then only the OLDEST keys go
 * ({@link QUESTION_NOTIFY_LEDGER_LIMIT}). A question that disappears and comes
 * back under the same id — an answer the agent refused, raised again — keeps
 * its key and is not notified again. The ledger lives in this window's memory,
 * so a restart of the app can notify a still-waiting question once more.
 */
import { formatElapsedSeconds, questionAgeSeconds } from "./questionAge";

/** One question waiting on one agent, as the notifier needs it. */
export interface WaitingQuestion {
  deckId: string;
  agentId: string;
  questionId: string;
  /** When the agent raised it; a question with no time is never notified. */
  raisedAtMs?: number;
  /** The tile's name for the agent. */
  agentName: string;
  /** The first question's prompt, as the tile would show it. */
  prompt: string;
}

/** A notification to post. */
export interface QuestionNotification {
  key: string;
  title: string;
  body: string;
}

/** The most keys the ledger keeps before forgetting the oldest. */
export const QUESTION_NOTIFY_LEDGER_LIMIT = 1000;

/** The body's longest length, in characters, ellipsis included. */
export const QUESTION_NOTIFY_BODY_LIMIT = 120;

/** What has been notified, oldest first. */
export type QuestionNotifyLedger = Set<string>;

/** The ledger key for one question on one agent of one daemon. */
export function questionNotifyKey(question: Pick<WaitingQuestion, "deckId" | "agentId" | "questionId">): string {
  return JSON.stringify([question.deckId, question.agentId, question.questionId]);
}

function truncate(text: string, limit: number): string {
  const chars = Array.from(text);
  return chars.length <= limit ? text : `${chars.slice(0, limit - 1).join("")}…`;
}

/**
 * The notifications now due, each recorded in `ledger` as it is returned, so
 * the caller posts every one it gets and a second call at the same instant
 * returns none.
 */
export function dueQuestionNotifications(
  waiting: WaitingQuestion[],
  nowMs: number,
  afterMinutes: number,
  ledger: QuestionNotifyLedger,
): QuestionNotification[] {
  if (!Number.isFinite(afterMinutes) || afterMinutes <= 0) return [];
  const afterSeconds = afterMinutes * 60;
  const due: QuestionNotification[] = [];
  for (const question of waiting) {
    if (question.raisedAtMs === undefined || question.raisedAtMs <= 0) continue;
    const key = questionNotifyKey(question);
    if (ledger.has(key)) continue;
    const age = questionAgeSeconds(question.raisedAtMs, nowMs);
    if (age < afterSeconds) continue;
    ledger.add(key);
    due.push({
      key,
      title: `Question waiting ${formatElapsedSeconds(age)}`,
      body: truncate(`${question.agentName}: ${question.prompt}`, QUESTION_NOTIFY_BODY_LIMIT),
    });
  }
  while (ledger.size > QUESTION_NOTIFY_LEDGER_LIMIT) {
    const oldest = ledger.values().next().value;
    if (oldest === undefined) break;
    ledger.delete(oldest);
  }
  return due;
}
