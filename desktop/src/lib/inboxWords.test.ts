import { describe, expect, it } from "vitest";
import type { AgentInbox, AgentSession, WakeHold } from "../types";
import { deckAttentionText, inboxAttentionWords, INBOX_WORDS } from "./inboxWords";

function inbox(queued: number, wakeHold?: WakeHold): AgentInbox {
  return { queued, fetched: 0, undeliverable: 0, wake: "doorbell", ...(wakeHold ? { wakeHold } : {}) };
}

function agent(role: string, isStartRole: boolean, agentInbox?: AgentInbox): AgentSession {
  return { id: role, role, isStartRole, ...(agentInbox ? { inbox: agentInbox } : {}) } as AgentSession;
}

// PRD #1666 part 3, round 5-wake (Q6): the header's attention item. The TUI's
// `deck_attention_text` tests in `src/inbox_display.rs` pin the same strings.
describe("deck attention words", () => {
  /// Scenario: A held start role is named with its hold's words and every other held agent is counted, in the strings the TUI pins too.
  it("names one held orchestrator and counts the rest", () => {
    expect(deckAttentionText([
      agent("Coder", false, inbox(1, "not_taken")),
      agent("Captain", true, inbox(1, "possible_draft")),
      agent("Quiet", false, inbox(3)),
    ])).toBe("✉ Captain: waiting: there may be an unsent draft. Send it, or type x then Ctrl+C. · 1 other held");
    expect(deckAttentionText([agent("Captain", true, inbox(1, "paste_open"))]))
      .toBe("✉ Captain: waiting: a paste into its input has not finished.");
    expect(deckAttentionText([agent("A", false, inbox(1, "cannot_type")), agent("B", false, inbox(2, "awaiting_prompt"))])).toBe("✉ 2 held");
    expect(deckAttentionText([agent("Captain", true, inbox(2))])).toBeUndefined();
  });

  /// Scenario: An agent's attention words are its hold's words while mail waits, the stall's for an unknown hold, the standalone warnings with nothing waiting, and nothing otherwise.
  it("gives each hold the words its card uses", () => {
    expect(inboxAttentionWords(inbox(2, "cannot_type"))).toBe(INBOX_WORDS.wakeHolds.cannot_type);
    expect(inboxAttentionWords(inbox(1, "unknown"))).toBe("waiting to wake: send it a prompt");
    expect(inboxAttentionWords(inbox(0, "dialog_during_ring"))).toBe(INBOX_WORDS.wakeHoldsAlone.dialog_during_ring);
    expect(inboxAttentionWords(inbox(0, "possible_draft"))).toBeUndefined();
    expect(inboxAttentionWords(inbox(0, "unknown"))).toBeUndefined();
    expect(inboxAttentionWords(inbox(3))).toBeUndefined();
    expect(inboxAttentionWords(undefined)).toBeUndefined();
  });
});
