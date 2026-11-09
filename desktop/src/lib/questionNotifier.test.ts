import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./bridge";
import { dueQuestionNotifications, QUESTION_NOTIFY_BODY_LIMIT, type QuestionNotifyLedger, type WaitingQuestion } from "./questionNotifier";
import { QUESTION_NOTIFY_TICK_MS, useQuestionNotifications } from "../hooks/useQuestionNotifications";
import type { DeckFleet } from "../types";

const RAISED = 1_767_225_600_000;
const MINUTE = 60_000;

function waiting(overrides: Partial<WaitingQuestion> = {}): WaitingQuestion {
  return {
    deckId: "deck-a", agentId: "7", questionId: "q-1", raisedAtMs: RAISED,
    agentName: "reviewer", prompt: "Which colour?", ...overrides,
  };
}

describe("one notification per waiting question (issue #2075)", () => {
  /**
   * Scenario (question/desktop/036): a question has waited on one agent; with
   * N = 10 minutes nothing is posted before ten minutes, one notification is
   * posted at ten minutes titled with the wait and naming the agent and the
   * prompt, and nothing more is posted for it at any later check.
   */
  it("question/desktop/036: none before N minutes, one at N, none after", () => {
    const ledger: QuestionNotifyLedger = new Set();
    expect(dueQuestionNotifications([waiting()], RAISED + 9 * MINUTE + 59_000, 10, ledger)).toEqual([]);
    const due = dueQuestionNotifications([waiting()], RAISED + 10 * MINUTE, 10, ledger);
    expect(due).toHaveLength(1);
    expect(due[0].title).toBe("Question waiting 10m");
    expect(due[0].body).toBe("reviewer: Which colour?");
    expect(dueQuestionNotifications([waiting()], RAISED + 10 * MINUTE, 10, ledger)).toEqual([]);
    expect(dueQuestionNotifications([waiting()], RAISED + 6 * 60 * MINUTE, 10, ledger)).toEqual([]);
  });

  /**
   * Scenario (question/desktop/036): a question answered at nine minutes is
   * gone from the waiting list at ten, so it is never notified; the same
   * question id raised again later, after it was notified once, is not
   * notified again.
   */
  it("question/desktop/036: an answered question never notifies, and one question never notifies twice", () => {
    const ledger: QuestionNotifyLedger = new Set();
    expect(dueQuestionNotifications([waiting()], RAISED + 9 * MINUTE, 10, ledger)).toEqual([]);
    expect(dueQuestionNotifications([], RAISED + 10 * MINUTE, 10, ledger)).toEqual([]);
    expect(dueQuestionNotifications([], RAISED + 60 * MINUTE, 10, ledger)).toEqual([]);

    const raisedAgain = waiting({ questionId: "q-2" });
    expect(dueQuestionNotifications([raisedAgain], RAISED + 10 * MINUTE, 10, ledger)).toHaveLength(1);
    expect(dueQuestionNotifications([], RAISED + 11 * MINUTE, 10, ledger)).toEqual([]);
    expect(dueQuestionNotifications([raisedAgain], RAISED + 30 * MINUTE, 10, ledger)).toEqual([]);
  });

  /**
   * Scenario (question/desktop/036): with N = 0 nothing is ever posted; a
   * question with no time to count from is never posted; the same question id
   * on two agents, or on two daemons, is two questions.
   */
  it("question/desktop/036: N = 0 is off, an untimed question is skipped, and the key names daemon and pane", () => {
    expect(dueQuestionNotifications([waiting()], RAISED + 600 * MINUTE, 0, new Set())).toEqual([]);
    expect(dueQuestionNotifications([waiting({ raisedAtMs: undefined })], RAISED + 600 * MINUTE, 10, new Set())).toEqual([]);
    const due = dueQuestionNotifications(
      [waiting(), waiting({ agentId: "8" }), waiting({ deckId: "deck-b" })],
      RAISED + 10 * MINUTE,
      10,
      new Set(),
    );
    expect(due).toHaveLength(3);
    expect(new Set(due.map((n) => n.key)).size).toBe(3);
  });

  it("question/desktop/036: the body is cut to its limit with an ellipsis", () => {
    const [due] = dueQuestionNotifications([waiting({ prompt: "x".repeat(300) })], RAISED + 10 * MINUTE, 10, new Set());
    expect(Array.from(due.body)).toHaveLength(QUESTION_NOTIFY_BODY_LIMIT);
    expect(due.body.endsWith("…")).toBe(true);
  });
});

describe("the notifier hook (issue #2075)", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function fleetWith(question: { id: string; raisedAtMs?: number } | undefined): DeckFleet {
    return [{
      agents: [{
        id: "7", daemonId: "deck-a", displayName: "reviewer",
        pendingQuestion: question && {
          id: question.id, kind: "choice", channel: "held", answerable: true, raisedAtMs: question.raisedAtMs,
          questions: [{ prompt: "Which colour?", multiSelect: false, options: [] }],
        },
      }],
    }] as unknown as DeckFleet;
  }

  /**
   * Scenario (question/desktop/036): the app is open with a question waiting
   * on one agent and the setting at its default of ten minutes; a check on the
   * timer at ten minutes posts one notification, later ticks post nothing, and
   * with the setting at Off nothing is posted at all.
   */
  it("question/desktop/036: posts once from the timer at the default ten minutes, and never when Off", () => {
    let now = RAISED;
    const post = vi.fn();
    const settings: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
    renderHook(() => useQuestionNotifications(fleetWith({ id: "q-1", raisedAtMs: RAISED }), settings, post, () => now));
    expect(post).not.toHaveBeenCalled();
    now = RAISED + 10 * MINUTE;
    vi.advanceTimersByTime(QUESTION_NOTIFY_TICK_MS);
    expect(post).toHaveBeenCalledTimes(1);
    expect(post).toHaveBeenCalledWith("Question waiting 10m", "reviewer: Which colour?");
    now = RAISED + 60 * MINUTE;
    vi.advanceTimersByTime(QUESTION_NOTIFY_TICK_MS * 4);
    expect(post).toHaveBeenCalledTimes(1);

    const off = vi.fn();
    renderHook(() => useQuestionNotifications(
      fleetWith({ id: "q-9", raisedAtMs: RAISED }),
      { ...DEFAULT_DESKTOP_SETTINGS, questions: { notify: 0 } },
      off,
      () => RAISED + 600 * MINUTE,
    ));
    vi.advanceTimersByTime(QUESTION_NOTIFY_TICK_MS * 4);
    expect(off).not.toHaveBeenCalled();
  });
});
