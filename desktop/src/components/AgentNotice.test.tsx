import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import { AgentTile } from "./AgentTile";
import { mapDesktopSnapshot, type DesktopSnapshotDto } from "../lib/bridge";
import type { AgentSession } from "../types";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// PRD #2050: the daemon's notice about a way an OpenCode agent runs that the
// deck cannot fully support. The sentence is composed by the daemon
// (`src/agent_notice.rs`) and rides the card's notices, so the tile shows it
// verbatim in the row the TUI's card prints it in.
const SHARED_SERVICE =
  "OpenCode 2 is using its shared background service, so the deck cannot follow this agent: add --standalone to the agent's command, or run opencode service set disabled true, then restart the agent";

function snapshot(inbox: Record<string, unknown>): DesktopSnapshotDto {
  const agent = {
    id: "9", paneId: "oc-pane", displayName: "Auditor", agentType: "open_code" as const, incarnation: "listed-notice-process",
    rows: 24, cols: 80, status: "idle" as const, toolCount: 0,
    tab: { kind: "orchestration" as const, name: "test", roleIndex: 2, roleName: "auditor", isStartRole: false },
    inbox: { queued: 0, fetched: 0, undeliverable: 0, wake: "none", ...inbox },
  };
  return {
    connection: { status: "connected", deckId: "deck-0000000000002050", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 12, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 12, source: "daemon",
  };
}

function tile(agent: AgentSession) {
  render(<AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()}
    onInboxAction={vi.fn(async () => ({ outcome: "applied" }))} />);
  return within(screen.getByRole("article", { name: "Auditor" }));
}

describe("an OpenCode agent the deck cannot fully support (PRD #2050)", () => {
  /// Scenario: The daemon reports an OpenCode 2 agent on the shared background service while its integration has not connected; the tile's notice row shows the daemon's whole sentence naming both fixes, in place of the not-connected notice.
  it("shows the daemon's notice whole, ahead of the integration notice", () => {
    const card = tile(mapDesktopSnapshot(snapshot({ agentNotice: SHARED_SERVICE, integration: "not_armed" })).agents[0]);
    const row = card.getByTestId("inbox-notice-9");
    expect(row).toHaveTextContent(SHARED_SERVICE);
    expect(row).not.toHaveTextContent(words.notArmed);
  });

  /// Scenario: The daemon sends no notice, or an empty one; the tile's notice row falls back to what it showed before.
  it("leaves the row to the other notices when there is none", () => {
    let card = tile(mapDesktopSnapshot(snapshot({ integration: "not_armed" })).agents[0]);
    expect(card.getByTestId("inbox-notice-9")).toHaveTextContent(words.notArmed);
    document.body.innerHTML = "";
    card = tile(mapDesktopSnapshot(snapshot({ agentNotice: "   " })).agents[0]);
    expect(card.queryByTestId("inbox-notice-9")).toBeNull();
  });
});
