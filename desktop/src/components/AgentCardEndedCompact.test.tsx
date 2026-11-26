import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { placedCardKey, type PlacedCard } from "../lib/agentCards";
import { ENDED_ROLE_WORDS } from "../lib/endedRoles";
import type { AgentCardEntry, EndedRole } from "../types";
import { AgentCardPanel } from "./AgentCardPanel";
import { AgentCardsDeckBody } from "./AgentCardsGrid";

/*
 * Issue #1965 on the compact card of issue #2418: an exited orchestration
 * role's reason sits on the card's one-line status line, at any depth of the
 * tree, and the agent pane's Card tab shows the reason and the last output
 * in full.
 */

function card(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): PlacedCard {
  const entry = {
    key: `pane-${agentId}/${agentId}`,
    instance_id: "daemon-instance-ended",
    pane_id: `pane-${agentId}`,
    agent_id: agentId,
    agent_type: null,
    name,
    status: "stopped",
    needs_you: false,
    needs_you_reasons: [],
    group: { id: "orchestration:o-1", kind: "orchestration", label: "mixed" },
    relations: {},
    report: null,
    fields: [],
    questions: [],
    actions: [],
    primary_action_id: null,
    facts: { source: "deck", pull_request: null },
    revision: 1,
    updated_at: "2026-11-22T00:00:00Z",
    ...overrides,
  } as unknown as AgentCardEntry;
  return { deckId: "deck-a", entry };
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
  lastOutput: ["error: launcher failed", "Cannot find module './usage-file.ts'"],
};

describe("an exited role on the compact card (issue #1965, issue #2418)", () => {
  /// Scenario: The Cards tree shows an orchestrator with its reviewer role opened under it; the reviewer's command exited at
  /// spawn. The reviewer's card, one level down, says Exited, why and its last output on its status line, not on a line of its
  /// own, and a click on that text opens the agent like the rest of the card.
  it("says why on the status line of a card under another, and opens the agent's card from there", () => {
    const orchestrator = card("1", "orchestrator", { status: "idle" });
    const reviewer = card("4", "reviewer", { parent: orchestrator.entry.key });
    const showAgentCard = vi.fn(() => undefined);
    render(
      <AgentCardsDeckBody
        deckId="deck-a"
        cards={[orchestrator, reviewer]}
        tree
        openCards={new Set([placedCardKey(orchestrator)])}
        needsYouOnly={false}
        controls={{ showAgentCard } as never}
        endedRoles={[REVIEWER]}
      />,
    );
    const ended = screen.getAllByTestId("agent-card-ended");
    expect(ended).toHaveLength(1);
    const reviewerCard = ended[0].closest("article")!;
    expect(reviewerCard).toHaveAttribute("data-depth", "1");
    expect(reviewerCard).toHaveAttribute("data-card-key", reviewer.entry.key);
    const statusLine = within(reviewerCard).getByTestId("agent-card-details");
    expect(statusLine).toContainElement(ended[0]);
    expect(statusLine).toHaveAccessibleName(expect.stringContaining(`${ENDED_ROLE_WORDS.status}: its command exited (code 1)`));
    expect(ended[0]).toHaveTextContent(`${ENDED_ROLE_WORDS.status}: its command exited (code 1) · ${ENDED_ROLE_WORDS.lastOutput}: error: launcher failed ⏎ Cannot find module './usage-file.ts'`);
    fireEvent.click(ended[0]);
    expect(showAgentCard).toHaveBeenCalledTimes(1);
  });

  /// Scenario: The exited reviewer's pane is open with its Card tab. The Card tab shows Exited, the reason, and every line of the
  /// last output, each on its own line, where the card had to cut them short.
  it("shows the reason and every line of the last output in the Card tab", () => {
    const reviewer = card("4", "reviewer");
    render(<AgentCardPanel card={reviewer} name="reviewer" results={{}} tree controls={{ runCardAction: vi.fn(), openCardReference: vi.fn(), stepAgentCard: vi.fn() }} ended={REVIEWER} />);
    const section = screen.getByTestId("agent-card-panel-ended");
    expect(section).toHaveTextContent(ENDED_ROLE_WORDS.status);
    expect(section).toHaveTextContent("its command exited (code 1)");
    expect(section).toHaveTextContent(`${ENDED_ROLE_WORDS.lastOutput}:`);
    expect(section.querySelector("pre")?.textContent).toBe("error: launcher failed\nCannot find module './usage-file.ts'");
    expect(section).toHaveAttribute("title", REVIEWER.headline);
  });
});
