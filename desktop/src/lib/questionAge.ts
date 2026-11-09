/**
 * Issue #2075: how long a question an agent asked has waited, in the words
 * both clients use.
 *
 * The TUI's card and this app's tile show the same badge, `QUESTION <age>`
 * (CLAUDE.md rule 22), so the age is written exactly as the TUI writes its
 * card's elapsed times: `format_elapsed` in `src/ui.rs`. Below a minute it is
 * seconds (`0s`, `3s`), below an hour minutes with any leftover seconds
 * (`1m`, `1m 30s`), and from an hour on hours with any leftover minutes
 * (`1h`, `1h 5m`, `26h 3m`) — there is no day unit. A start ahead of now
 * (clock skew between the agent's machine and this one) reads `0s`.
 * `questionAge.test.ts` pins the same examples as the Rust test.
 */
import type { PendingQuestionDto } from "./voiceQuestion";

/** The badge's word, as the TUI card spells it. */
export const QUESTION_BADGE_WORD = "QUESTION";

/** `format_elapsed` from `src/ui.rs`, for a span of whole seconds. */
export function formatElapsedSeconds(totalSeconds: number): string {
  const total = Number.isFinite(totalSeconds) ? Math.max(0, Math.floor(totalSeconds)) : 0;
  if (total < 60) return `${total}s`;
  if (total < 3600) {
    const minutes = Math.floor(total / 60);
    const seconds = total % 60;
    return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`;
  }
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  return minutes === 0 ? `${hours}h` : `${hours}h ${minutes}m`;
}

/** Whole seconds from `raisedAtMs` to `nowMs`, never negative. */
export function questionAgeSeconds(raisedAtMs: number, nowMs: number): number {
  return Math.max(0, Math.floor((nowMs - raisedAtMs) / 1000));
}

/**
 * The tile's badge text for a waiting question: `QUESTION 12m`, or the word
 * alone when the daemon reported no time to count from.
 */
export function questionBadgeText(question: Pick<PendingQuestionDto, "raisedAtMs">, nowMs: number): string {
  if (question.raisedAtMs === undefined || question.raisedAtMs <= 0) return QUESTION_BADGE_WORD;
  return `${QUESTION_BADGE_WORD} ${formatElapsedSeconds(questionAgeSeconds(question.raisedAtMs, nowMs))}`;
}
