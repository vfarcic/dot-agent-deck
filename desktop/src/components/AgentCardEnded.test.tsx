import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { AgentCardEntry, EndedRole } from "../types";
import { ENDED_ROLE_WORDS } from "../lib/endedRoles";
import { AgentCardsDeckBody } from "./AgentCardsGrid";

function stoppedCard(agentId: string, name: string): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`,
    instance_id: "daemon-instance-ended",
    pane_id: `pane-${agentId}`,
    agent_id: agentId,
    agent_type: null,
    name,
    status: "stopped",
    needs_you: true,
    needs_you_reasons: ["stopped"],
    group: { id: "orchestration:o-1", kind: "orchestration", label: "mixed" },
    relations: {},
    report: null,
    fields: [],
    questions: [],
    actions: [],
    primary_action_id: null,
    facts: { source: "deck", pull_request: null },
    revision: 1,
    updated_at: "2026-11-21T00:00:00Z",
  } as unknown as AgentCardEntry;
}

const REVIEWER: EndedRole = {
  paneId: "pane-4",
  agentId: "4",
  roleIndex: 1,
  roleName: "reviewer",
  orchestrationId: "o-1",
  orchestrationName: "mixed",
  reason: "its command exited (code 1)",
  headline: "reviewer exited: its command exited (code 1)",
  lastOutput: ["Cannot find module './usage-file.ts'"],
};

describe("an exited role's card in the Cards view (issue #1965)", () => {
  /// Scenario: The Cards view shows a stopped card for the reviewer role, whose command exited at spawn. Its card says Exited,
  /// why, and the last line its pane printed, in the words its table row uses; another stopped card, whose agent is a different
  /// generation of the same pane, says nothing extra.
  it("says why the role exited on its stopped card", () => {
    const cards = [stoppedCard("4", "reviewer"), stoppedCard("5", "coder")].map((entry) => ({ deckId: "deck-a", entry }));
    const stale = { ...REVIEWER, paneId: "pane-5", agentId: "9" };
    render(<AgentCardsDeckBody deckId="deck-a" cards={cards} needsYouOnly={false} controls={{} as never} endedRoles={[REVIEWER, stale]} />);
    const ended = screen.getAllByTestId("agent-card-ended");
    expect(ended).toHaveLength(1);
    expect(ended[0]).toHaveTextContent(`${ENDED_ROLE_WORDS.status}: its command exited (code 1)`);
    expect(ended[0]).toHaveTextContent(`${ENDED_ROLE_WORDS.lastOutput}:`);
    expect(ended[0]).toHaveTextContent("Cannot find module './usage-file.ts'");
    expect(ended[0]).toHaveAttribute("title", "reviewer exited: its command exited (code 1)");
  });
});
