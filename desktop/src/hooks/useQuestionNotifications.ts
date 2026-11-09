/**
 * Issue #2075 — post ONE operating-system notification for each question an
 * agent on any observed daemon has left unanswered for N minutes.
 *
 * Mounted once, in `DeckShell`, beside the settings it reads, so it runs on
 * every screen and for every deck in the fleet rather than only the selected
 * one. The decision is `dueQuestionNotifications`' (pure, and tested there);
 * this hook only supplies the fleet, the clock and the setting, and keeps the
 * ledger for the life of the window. It re-reads on every fleet change and on a
 * timer, so a question that has been waiting quietly is noticed within one
 * tick of its N minutes without any daemon event to prompt it.
 */
import { useEffect, useRef } from "react";
import type { DesktopSettingsDto } from "../lib/bridge";
import { questionNotifyAfterMinutes } from "../lib/bridge";
import { sanitizeText } from "../lib/displayText";
import { postOsNotification } from "../lib/osNotification";
import { dueQuestionNotifications, type QuestionNotifyLedger, type WaitingQuestion } from "../lib/questionNotifier";
import type { DeckFleet } from "../types";

/** How often the hook re-reads the clock between fleet changes. */
export const QUESTION_NOTIFY_TICK_MS = 15_000;

/**
 * Every question waiting in `fleet`, as the notifier reads it: on a deck the
 * app is connected to, so a cached question on a deck it can no longer reach
 * is not taken as still waiting, and not one whose answer the daemon has
 * already accepted and is delivering (PR #2208 review).
 */
export function waitingQuestionsOf(fleet: DeckFleet): WaitingQuestion[] {
  return fleet.filter((deck) => deck.connection?.status === "connected").flatMap((deck) =>
    deck.agents.flatMap((agent): WaitingQuestion[] => {
      const question = agent.pendingQuestion;
      if (!question || question.answerQueued) return [];
      return [{
        deckId: agent.daemonId,
        agentId: agent.id,
        questionId: question.id,
        raisedAtMs: question.raisedAtMs,
        agentName: sanitizeText(agent.displayName),
        prompt: sanitizeText(question.questions[0]?.prompt ?? ""),
      }];
    }),
  );
}

/**
 * `ready` is whether the saved settings have been read: until then `settings`
 * holds the defaults, so a check would decide on a delay the user may have
 * changed or turned off (PR #2208 review). Nothing is checked before it.
 */
export function useQuestionNotifications(
  fleet: DeckFleet,
  settings: DesktopSettingsDto,
  ready: boolean,
  post: (title: string, body: string) => boolean | void | Promise<boolean | void> = postOsNotification,
  now: () => number = Date.now,
): void {
  const ledger = useRef<QuestionNotifyLedger>(new Set());
  const latest = useRef({ fleet, settings, post, now });
  latest.current = { fleet, settings, post, now };

  useEffect(() => {
    if (!ready) return;
    const check = () => {
      const { fleet: currentFleet, settings: currentSettings, post: currentPost, now: clock } = latest.current;
      const due = dueQuestionNotifications(
        waitingQuestionsOf(currentFleet),
        clock(),
        questionNotifyAfterMinutes(currentSettings),
        ledger.current,
      );
      // The ledger already holds each key, so a post still in flight is not
      // repeated; one that failed gives its key back and is tried again on
      // the next check (PR #2208 review).
      for (const notification of due) {
        const forget = () => ledger.current.delete(notification.key);
        Promise.resolve()
          .then(() => currentPost(notification.title, notification.body))
          .then((posted) => {
            if (posted === false) forget();
          }, forget);
      }
    };
    check();
    const timer = window.setInterval(check, QUESTION_NOTIFY_TICK_MS);
    return () => window.clearInterval(timer);
  }, [fleet, settings, ready]);
}
