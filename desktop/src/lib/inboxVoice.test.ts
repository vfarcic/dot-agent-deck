import { describe, expect, it, vi } from "vitest";
import type { ConfirmState } from "../components/ConfirmDialog";
import { createFixtureSnapshot } from "../data/fixture";
import type { AgentSession, InboxAttentionItem } from "../types";
import {
  confirmInboxActionFor,
  INBOX_CONFIRMATION_ALREADY_OPEN,
  INBOX_NEEDS_ONE_DAEMON,
  INBOX_TARGET_GONE,
  INBOX_TARGET_RESTARTED,
  inboxActionConfirmation,
  inboxNothingToRetry,
  inboxNothingWaiting,
} from "./inboxVoice";
import { INBOX_WORDS } from "./inboxWords";

/* Issue #2148 — Retry and Cancel on an agent's deck messages, by voice. */

function item(messageId: string, kind: InboxAttentionItem["kind"], revision = 3): InboxAttentionItem {
  return { messageId, revision, kind, fromLabel: "the orchestrator", subject: `review ${messageId}` };
}

function agentWith(attention: InboxAttentionItem[], patch: Partial<AgentSession> = {}): AgentSession {
  const base = createFixtureSnapshot("connected").agents[0];
  return {
    ...base,
    displayName: "Coder",
    tab: { kind: "dashboard" },
    paneId: "worker",
    incarnation: "process-a",
    inbox: { ...(base.inbox ?? {}), attention } as AgentSession["inbox"],
    ...patch,
  };
}

const confirmation = (value: ConfirmState | string): ConfirmState => {
  if (typeof value === "string") throw new Error(`refused: ${value}`);
  return value;
};

describe("a spoken Retry or Cancel on deck messages (issue #2148)", () => {
  /// Scenario: The tester's tile lists no deck message waiting on the user; a spoken Retry or Cancel opens nothing and says nothing is waiting.
  it("refuses when nothing on the agent's tile waits on Retry or Cancel", () => {
    const agent = agentWith([]);
    for (const verb of ["retry", "cancel"] as const) {
      expect(inboxActionConfirmation(verb, agent, vi.fn(), vi.fn())).toBe(inboxNothingWaiting("Coder"));
    }
  });

  /// Scenario: The only message on the tile is out of attempts; a spoken Retry opens nothing and says the message can only be cancelled, while a spoken Cancel opens its confirmation.
  it("does not retry a message out of attempts, which the tile offers only Cancel on", () => {
    const agent = agentWith([item("m-1", "out_of_attempts")]);
    expect(inboxActionConfirmation("retry", agent, vi.fn(), vi.fn())).toBe(inboxNothingToRetry("Coder", 1));
    expect(confirmation(inboxActionConfirmation("cancel", agent, vi.fn(), vi.fn())).title).toBe("Cancel the deck message to Coder?");
  });

  /// Scenario: The tile lists one uncertain message and one out of attempts; the Retry confirmation lists only the uncertain one by sender and subject, says the other is left for Cancel, and sends nothing until its button is pressed.
  it("lists what a Retry covers by sender and subject and sends nothing until pressed", async () => {
    const inboxAction = vi.fn(async () => ({ outcome: "applied" }));
    const onAnswer = vi.fn();
    const question = confirmation(inboxActionConfirmation("retry", agentWith([item("m-1", "uncertain"), item("m-2", "out_of_attempts")]), inboxAction, onAnswer));
    expect(question.title).toBe("Retry the deck message to Coder?");
    expect(question.body).toContain("from the orchestrator: “review m-1”");
    expect(question.body).not.toContain("review m-2");
    expect(question.body).toContain("One more message is out of attempts, so it is left for Cancel.");
    expect(question.label).toBe(INBOX_WORDS.retry);
    expect(question.neutral).toBe(true);
    expect(inboxAction).not.toHaveBeenCalled();

    await question.action({ keepWorktree: false });
    expect(inboxAction).toHaveBeenCalledTimes(1);
    expect(inboxAction).toHaveBeenCalledWith(
      { deckId: agentWith([]).daemonId, agentId: agentWith([]).id, incarnation: "process-a" },
      { paneId: "worker", messageId: "m-1", revision: 3 },
      "retry",
    );
    expect(onAnswer).toHaveBeenCalledWith(`Retry for Coder: ${INBOX_WORDS.retryApplied}.`);
  });

  /// Scenario: The tile lists two messages; the Cancel confirmation is destructive, names both, and its button cancels each at the revision shown, reporting each message's own outcome.
  it("cancels every listed message at the revision shown and reports each outcome", async () => {
    const inboxAction = vi.fn(async (_target: unknown, message: { messageId: string }, _verb: string) => ({ outcome: message.messageId === "m-1" ? "applied" : "stale" }));
    const onAnswer = vi.fn();
    const question = confirmation(inboxActionConfirmation("cancel", agentWith([item("m-1", "uncertain", 4), item("m-2", "not_started", 9)]), inboxAction, onAnswer));
    expect(question.title).toBe("Cancel 2 deck messages to Coder?");
    expect(question.neutral).toBeUndefined();
    expect(question.label).toBe("Cancel messages");
    expect(question.cancelLabel).toBe("Keep them");
    expect(question.body).toContain("review m-1");
    expect(question.body).toContain("review m-2");

    await question.action({ keepWorktree: false });
    expect(inboxAction.mock.calls.map(([, message, verb]) => [message, verb])).toEqual([
      [{ paneId: "worker", messageId: "m-1", revision: 4 }, "cancel"],
      [{ paneId: "worker", messageId: "m-2", revision: 9 }, "cancel"],
    ]);
    expect(onAnswer).toHaveBeenCalledWith(
      `Cancel for Coder: from the orchestrator: “review m-1”: ${INBOX_WORDS.cancelApplied}; from the orchestrator: “review m-2”: ${INBOX_WORDS.actionStale}.`,
    );
  });

  /// Scenario: The deck screen is asked to confirm a spoken Retry while a confirmation is open, under All daemons, for an agent no longer listed and for one whose daemon restarted; each is refused in words and nothing opens.
  it("refuses over an open confirmation, under All daemons, and for a gone or restarted agent", () => {
    const agent = agentWith([item("m-1", "uncertain")]);
    const inboxAction = vi.fn(async () => ({ outcome: "applied" }));
    const screen = { agents: [agent], allDecks: false, confirmationOpen: false, inboxAction };
    const target = { deckId: agent.daemonId, agentId: agent.id, incarnation: "process-a" };
    expect(confirmInboxActionFor("retry", target, { ...screen, confirmationOpen: true }, vi.fn())).toBe(INBOX_CONFIRMATION_ALREADY_OPEN);
    expect(confirmInboxActionFor("retry", target, { ...screen, allDecks: true }, vi.fn())).toBe(INBOX_NEEDS_ONE_DAEMON);
    expect(confirmInboxActionFor("retry", { ...target, agentId: "nobody" }, screen, vi.fn())).toBe(INBOX_TARGET_GONE);
    expect(confirmInboxActionFor("retry", { ...target, incarnation: "process-b" }, screen, vi.fn())).toBe(INBOX_TARGET_RESTARTED);
    expect(confirmation(confirmInboxActionFor("retry", target, screen, vi.fn())).title).toBe("Retry the deck message to Coder?");
    expect(inboxAction).not.toHaveBeenCalled();
  });
});
