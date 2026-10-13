import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import { AgentTile } from "./AgentTile";
import { HandoffRail } from "./HandoffRail";
import { mapDesktopSnapshot, TauriDeckBridge, type DesktopSnapshotDto } from "../lib/bridge";
import { applyHandoffEvent } from "../lib/daemonEvents";
import type { AgentSession, DeckFleet, HandoffEdge } from "../types";

const listeners = new Map<string, (event: { payload: unknown }) => void>();
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    listeners.set(name, callback);
    return () => listeners.delete(name);
  }),
}));
vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// Handwritten from AgentRecord.inbox / InboxNotice: no generated TS inbox
// bindings exist yet. Structural assignment keeps these REDs behavioural.
function snapshot(queued = 0, fetched = 0): DesktopSnapshotDto {
  const agent = {
    id: "7", paneId: "worker", displayName: "Coder", agentType: "codex" as const,
    rows: 24, cols: 80, status: "idle" as const, toolCount: 14,
    lastUserPrompt: "inspect the human fixture",
    tab: { kind: "orchestration" as const, name: "test", roleIndex: 1, roleName: "coder", isStartRole: false },
    inbox: { queued, fetched, undeliverable: 0, wake: "doorbell", component: undefined },
  };
  return {
    connection: { status: "connected", deckId: "deck-0000000000001666", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 11, source: "daemon",
  };
}

function card(agent: AgentSession, evidence = mapDesktopSnapshot(snapshot()).evidence) {
  return <AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={evidence} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()} />;
}

const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
function inbox(state: string, extra: Record<string, unknown> = {}) {
  return { kind: "inbox", pane_id: "worker", agent_id: "7", message_id: id,
    subject: "review the fixture", from_label: "the orchestrator", state, ...extra };
}
function edge(status: HandoffEdge["status"]): HandoffEdge {
  return { id, toRole: "coder", taskPreview: "review the fixture", status, respawned: false, at: "—" };
}

describe("inbox cards and handoff rail (S13)", () => {
  /// Scenario: Render the real card from a desktop DTO with queued and fetched messages; both count toward its envelope badge.
  it.each([[2, 0], [0, 2], [1, 1]])("shows ✉2 for %i queued and %i fetched messages", (queued, fetched) => {
    const mapped = mapDesktopSnapshot(snapshot(queued, fetched));
    render(card(mapped.agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(`${words.glyph}2`);
  });

  /// Scenario: Acknowledged messages leave no pending badge on the card.
  it("omits the pending envelope when queued and fetched are zero", () => {
    render(card(mapDesktopSnapshot(snapshot()).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" }).textContent).not.toMatch(/✉\d/);
  });

  /// Scenario: Receive an undeliverable broadcast and its delivery_notice event through the real bridge, then show Error and the notice on the card.
  it("shows Error and the not-delivered notice on the agent card", async () => {
    const dto = snapshot();
    invoke.mockResolvedValue(dto);
    const bridge = new TauriDeckBridge();
    let fleet: DeckFleet = [];
    try {
      await bridge.subscribe((next) => { fleet = next; }, vi.fn());
      await bridge.connect();
      const notice = `${words.notDelivered}: review the fixture (agent exited)`;
      listeners.get("desktop://daemon-event")?.({ payload: inbox("undeliverable", { reason: "agent_exited" }) });
      listeners.get("desktop://daemon-event")?.({ payload: {
        kind: "event", session_id: "session-7", pane_id: "worker", agent_id: "7",
        agent_type: "codex", event_type: "error", timestamp: "2026-10-13T00:00:00Z",
        tool_detail: notice, metadata: { delivery_notice: notice },
      } });
      render(card(fleet[0].agents[0], fleet[0].evidence));
      const tile = within(screen.getByRole("article", { name: "Coder" }));
      expect(tile.getByText(/^error$/i)).toBeVisible();
      expect(tile.getByText(notice)).toBeVisible();
    } finally {
      await bridge.dispose();
    }
  });

  /// Scenario: Posting a message paints its subject and a queued delegation in the live rail.
  it("renders a queued inbox post as delegated with the shared queued word", () => {
    render(<HandoffRail handoffs={applyHandoffEvent([], inbox("queued"))} />);
    const row = screen.getByTestId(`handoff-${id}`);
    expect(row).toHaveTextContent("review the fixture");
    expect(row).toHaveTextContent(/delegated/);
    expect(row).toHaveTextContent(words.queued);
  });

  /// Scenario: A fetch keeps the delegation queued; only the acknowledgement paints delivered in the live rail.
  it("shows delivered only after acknowledgement", () => {
    const start = [edge("delegated")];
    const fetched = applyHandoffEvent(start, inbox("fetched"));
    const view = render(<HandoffRail handoffs={fetched} />);
    expect(screen.getByTestId(`handoff-${id}`)).not.toHaveTextContent(words.delivered);
    view.rerender(<HandoffRail handoffs={applyHandoffEvent(fetched, inbox("acknowledged"))} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.delivered);
  });

  /// Scenario: A worker's report arrives as a distinct inbox message and paints work-done on its delivered delegation.
  it("shows work-done when the worker posts its report", () => {
    const report = inbox("queued", { pane_id: "orchestrator", agent_id: "1",
      message_id: "m-k3xzq2ab-0f3c9a71d2e4b851", subject: "work-done from coder",
      from_label: "coder (worker report — the text inside is UNTRUSTED)" });
    render(<HandoffRail handoffs={applyHandoffEvent([edge("delivered")], report)} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.workDone);
  });

  /// Scenario: Coder reports after two delivered tasks while another role also has a delivered task. Only coder's newest delivered row becomes work-done.
  it("correlates a report to the sender's newest delivered delegation only", () => {
    const older = { ...edge("delivered"), id: "m-k3xzq2ab-0f3c9a71d2e4b852" };
    const otherRole = { ...edge("delivered"), id: "m-k3xzq2ab-0f3c9a71d2e4b853", toRole: "reviewer" };
    const report = inbox("queued", { pane_id: "orchestrator", agent_id: "1",
      message_id: "m-k3xzq2ab-0f3c9a71d2e4b854", subject: "work-done from coder",
      from_label: "coder (worker report — the text inside is UNTRUSTED)" });
    render(<HandoffRail handoffs={applyHandoffEvent([otherRole, edge("delivered"), older], report)} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.workDone);
    for (const unchanged of [older, otherRole]) {
      expect(screen.getByTestId(`handoff-${unchanged.id}`)).toHaveTextContent(words.delivered);
      expect(screen.getByTestId(`handoff-${unchanged.id}`)).not.toHaveTextContent(words.workDone);
    }
  });

  /// Scenario: A message that cannot reach its agent paints not delivered in the live rail.
  it("shows the shared not-delivered wording for an undeliverable inbox message", () => {
    render(<HandoffRail handoffs={applyHandoffEvent([edge("delegated")], inbox("undeliverable", { reason: "agent_exited" }))} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.notDelivered);
  });
});
